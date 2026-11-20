//! No Willow declarations or Cargo dependencies are needed.
pub struct Message {
    pub text: String,
    pub count: i64,
}
pub struct Counter {
    value: i64,
}
pub fn message(text: &str) -> Message {
    Message { text: text.to_owned(), count: 42 }
}
pub fn echo(value: Message) -> Message { value }
pub fn counter(value: i64) -> Box<Counter> { Box::new(Counter { value }) }
pub fn value(counter: &Counter) -> i64 { counter.value }
pub async fn greet(text: String) -> String { format!("Hello, {text}!") }
pub(crate) fn private_helper<T>(value: T) -> T { value }
