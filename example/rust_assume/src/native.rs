#[willow::assume(no_block)]
pub struct Handle(i64);

#[willow::assume(no_block)]
pub fn make() -> Box<Handle> {
    Handle::new()
}

impl Handle {
    #[willow::assume(no_block)]
    pub fn new() -> Box<Self> {
        Box::new(Self(42))
    }

    #[willow::assume(no_block)]
    pub fn read(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reviewed_methods_work_without_willow_compiler() {
        assert_eq!(super::make().read(), 42);
    }
}
