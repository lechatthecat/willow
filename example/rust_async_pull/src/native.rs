pub struct Source(event_source::Source);

impl Source {
    pub async fn new() -> Box<Self> {
        Box::new(Self(event_source::Source::default()))
    }

    pub async fn next_request(&self) -> Option<String> {
        self.0.next().await
    }
}

pub async fn respond(request: String) -> String {
    format!("handled {request}")
}
