use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

pub(super) const VERSION: &str = "4.15.4";
pub(super) struct Solver {
    child: Child,
    input: SyncSender<String>,
    written: Receiver<Result<(), String>>,
    output: Receiver<Result<String, String>>,
    pub transcript: String,
    deadline: Instant,
    timeout: Duration,
    poisoned: bool,
}
impl Drop for Solver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Solver {
    pub fn new(path: &std::path::Path, timeout_ms: u64) -> Result<Self, String> {
        let mut command = Command::new(path);
        command.args(["-in", "-smt2"]);
        Self::from_command(command, timeout_ms)
    }
    fn from_command(mut command: Command, timeout_ms: u64) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start pinned Z3 {VERSION}: {e}"))?;
        let mut input = child.stdin.take().ok_or("solver stdin unavailable")?;
        let output = child.stdout.take().ok_or("solver stdout unavailable")?;
        let (commands, requests) = sync_channel::<String>(1);
        let (sent, written) = sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(command) = requests.recv() {
                let result = input
                    .write_all(command.as_bytes())
                    .and_then(|_| input.flush())
                    .map_err(|e| format!("solver input error: {e}"));
                let failed = result.is_err();
                if sent.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let (tx, rx) = sync_channel(64);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = String::new();
                // Bound a malformed solver's individual response as well as the
                // aggregate collected by exchange.
                let read = std::io::Read::take(&mut reader, 1_048_576).read_line(&mut line);
                match read {
                    Ok(0) => break,
                    Ok(_) if line.len() < 1_048_576 => {
                        if tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = tx.send(Err("invalid or oversized solver output".into()));
                        break;
                    }
                }
            }
        });
        let mut s = Self {
            child,
            input: commands,
            written,
            output: rx,
            transcript: String::new(),
            deadline: Instant::now() + Duration::from_secs(60),
            timeout: Duration::from_millis(timeout_ms.max(1)),
            poisoned: false,
        };
        let version = s.exchange("(get-info :version)\n")?;
        if version.trim() != format!("(:version \"{VERSION}\")") {
            return Err(format!(
                "requires Z3 {VERSION}; received {}",
                version.trim()
            ));
        }
        s.exchange(&format!("(set-option :print-success false)\n(set-option :produce-models true)\n(set-option :memory_max_size 512)\n(set-option :smt.random_seed 0)\n(set-option :sat.random_seed 0)\n(set-option :timeout {timeout_ms})\n"))?;
        Ok(s)
    }
    pub fn exchange(&mut self, command: &str) -> Result<String, String> {
        if self.poisoned {
            return Err("solver session is invalid after an earlier error".into());
        }
        let result = self.exchange_inner(command);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn exchange_inner(&mut self, command: &str) -> Result<String, String> {
        if Instant::now() >= self.deadline {
            return Err("verification budget timeout".into());
        }
        if self.transcript.len().saturating_add(command.len()) > 64 * 1024 * 1024 {
            return Err("SMT byte budget exceeded".into());
        }
        self.transcript.push_str(command);
        let deadline = self
            .deadline
            .min(Instant::now() + self.timeout + Duration::from_secs(1));
        self.input
            .try_send(format!("{command}(echo \"willow-end\")\n"))
            .map_err(|_| "solver input unavailable".to_string())?;
        self.written
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "solver input timeout".to_string())??;
        let mut text = String::new();
        loop {
            let duration = deadline.saturating_duration_since(Instant::now());
            let line = self
                .output
                .recv_timeout(duration)
                .map_err(|_| "solver timeout or unexpected EOF".to_string())??;
            if line.trim() == "willow-end" {
                break;
            }
            text.push_str(&line);
            if text.len() > 1_048_576 {
                return Err("oversized solver response".into());
            }
        }
        if text.contains("(error") {
            return Err(format!("solver rejected VC: {}", text.trim()));
        }
        Ok(text)
    }
    pub fn query(
        &mut self,
        formula: &str,
        inputs: &[String],
    ) -> Result<(String, Option<String>), String> {
        let answer = self.exchange(&format!("(push 1)\n(assert {formula})\n(check-sat)\n"))?;
        let answer = answer.trim().to_string();
        if !matches!(answer.as_str(), "sat" | "unsat" | "unknown") {
            self.poisoned = true;
            return Err(format!("malformed solver status: {answer}"));
        }
        let model = if answer == "sat" && !inputs.is_empty() {
            Some(
                self.exchange(&format!("(get-value ({}))\n", inputs.join(" ")))?
                    .trim()
                    .into(),
            )
        } else {
            None
        };
        self.exchange("(pop 1)\n")?;
        Ok((answer, model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake(body: &str, timeout: u64) -> Result<Solver, String> {
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args(["-u", "-c", body]);
        Solver::from_command(command, timeout)
    }
    #[test]
    fn verification_solver_rejects_wrong_version_eof_and_malformed_output() {
        assert!(fake("print('(:version \"0.0\")'); print('willow-end')", 10).is_err());
        assert!(fake("pass", 10).is_err());
        let script = "import sys\nfor line in sys.stdin:\n if 'get-info' in line: print('(:version \"4.15.4\")')\n elif 'check-sat' in line: print('banana')\n elif 'echo' in line: print('willow-end')\n";
        let mut s = fake(script, 100).unwrap();
        assert!(s.query("true", &[]).unwrap_err().contains("malformed"));
    }
    #[test]
    fn verification_solver_timeout_is_bounded_and_poisoned() {
        let script = "import sys,time\nfor line in sys.stdin:\n if 'get-info' in line: print('(:version \"4.15.4\")')\n elif 'check-sat' in line: time.sleep(30)\n elif 'echo' in line: print('willow-end')\n";
        let mut s = fake(script, 1).unwrap();
        let start = Instant::now();
        assert!(s.query("true", &[]).unwrap_err().contains("timeout"));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(
            s.exchange("(check-sat)\n")
                .unwrap_err()
                .contains("earlier error")
        );
    }
}
