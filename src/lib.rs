pub mod error;

pub mod event_loop;
pub mod session;

#[cfg(target_arch = "wasm32")]
pub mod stomp_client_wasm {
    use std::future::Future;

    use crate::error::StompClientError;

    pub fn spawn<F: Future<Output = ()> + Send + 'static>(task: F) {
        wasm_bindgen_futures::futures_0_3::spawn_local(task);
    }
}
#[cfg(target_arch = "wasm32")]
pub use stomp_client_wasm::spawn;

#[cfg(not(target_arch = "wasm32"))]
pub mod stomp_client_tokio {
    use std::future::Future;

    pub fn spawn<F: Future<Output = ()> + Send + 'static>(task: F) {
        tokio::task::spawn(task);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use stomp_client_tokio::spawn;
