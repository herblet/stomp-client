use stomp_parser::error::StompParseError;

pub struct StompClientError {
    message: String,
}

impl StompClientError {
    pub fn new<T: Into<String>>(message: T) -> Self {
        StompClientError {
            message: message.into(),
        }
    }
}

impl std::fmt::Debug for StompClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("StompClientError: {}", self.message))
    }
}

impl From<StompParseError> for StompClientError {
    fn from(cause: StompParseError) -> Self {
        StompClientError::new(cause.message())
    }
}

#[cfg(not(target_arch = "wasm32"))]
use sender_sink::wrappers::SinkError;

#[cfg(not(target_arch = "wasm32"))]
impl From<SinkError> for StompClientError {
    fn from(source: SinkError) -> Self {
        StompClientError::new(format!("{:?}", source))
    }
}
