//! The public compiler pipeline uses the platform's small native stack.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-depth-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn compiler_pipeline_deep_contexts_on_one_mib() {
    std::thread::Builder::new().stack_size(1024*1024).spawn(|| {
        let project=Project::new();
        let mut source=String::from("fn id(x: i64) -> i64 { return x; }\n");
        let depth=1_000;
        for (index,(prefix,suffix)) in [
            ("id(",")"),
            ("true ? "," : 0"),
            ("match true { true => ",", false => 0 }"),
            ("[", "][0]"),
        ].into_iter().enumerate() {
            source.push_str(&format!("fn case{index}() -> i64 {{ return {}7{}; }}\n",prefix.repeat(depth),suffix.repeat(depth)));
        }
        source.push_str("fn main() { println(case0()); println(case1()); println(case2()); println(case3()); }\n");
        let input=project.0.join("main.wi");let output=project.0.join("program");
        std::fs::write(&input,source).unwrap();
        willow_compiler::compile(input.to_str().unwrap(),output.to_str().unwrap(),&willow_compiler::CompilerOptions::debug(),None).unwrap();
        let run=std::process::Command::new(&output).output().unwrap();
        assert!(run.status.success(),"{run:?}");
        assert_eq!(String::from_utf8_lossy(&run.stdout),"7\n7\n7\n7\n");
    }).unwrap().join().unwrap();
}
