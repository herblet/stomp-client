use async_executors::{SpawnHandle, YieldNow};
use futures::task::Spawn;

pub mod error;

mod event_loop;
pub mod session;

pub trait StompClientExecutor: SpawnHandle<()> + Spawn + YieldNow + Clone {}

impl<T: SpawnHandle<()> + Spawn + YieldNow + Clone> StompClientExecutor for T {}

#[cfg(target_arch = "wasm32")]
pub mod stomp_client_wasm {
    use super::StompClientExecutor;
    use async_executors::exec::Bindgen;

    pub fn executor() -> impl StompClientExecutor {
        Bindgen::new()
    }
}
#[cfg(target_arch = "wasm32")]
pub use stomp_client_wasm::executor;

#[cfg(not(target_arch = "wasm32"))]
pub mod stomp_client_tokio {
    use super::StompClientExecutor;
    use async_executors::exec::AsyncGlobal;

    pub fn executor() -> impl StompClientExecutor {
        AsyncGlobal::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use stomp_client_tokio::executor;
