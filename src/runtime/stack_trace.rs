#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFrame {
    pub function: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStackTrace {
    frames: Vec<RuntimeFrame>,
}

impl RuntimeStackTrace {
    pub fn push(&mut self, frame: RuntimeFrame) {
        self.frames.push(frame);
    }

    pub fn pop(&mut self) -> Option<RuntimeFrame> {
        self.frames.pop()
    }

    pub fn frames(&self) -> &[RuntimeFrame] {
        &self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_trace_records_source_frames() {
        let mut trace = RuntimeStackTrace::default();
        trace.push(RuntimeFrame {
            function: "main".to_string(),
            file: "main.wi".to_string(),
            line: 3,
            col: 5,
        });
        assert_eq!(trace.frames()[0].function, "main");
        assert_eq!(trace.pop().unwrap().line, 3);
    }
}
