pub use event_source::Source;
pub async fn source() -> Box<Source> {
    Box::default()
}
pub async fn next_request(source: crate::AsyncHandle<Source>) -> Option<String> {
    source.next().await
}
pub async fn respond(request: String) -> String {
    format!("handled {request}")
}
