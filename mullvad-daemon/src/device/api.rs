use std::pin::Pin;

use futures::{future::FusedFuture, Future};
use mullvad_types::{device::Device, wireguard::WireguardData};

use super::{Error, PrivateAccountAndDevice, ResponseTx};

pub(crate) struct CurrentApiCall {
    current_call: Option<Call>,
}

impl CurrentApiCall {
    pub fn new() -> Self {
        Self { current_call: None }
    }

    pub fn clear(&mut self) {
        self.current_call = None;
    }

    pub fn set_login(&mut self, login: ApiCall<PrivateAccountAndDevice>, tx: ResponseTx<()>) {
        self.current_call = Some(Call::Login(login, Some(tx)));
    }

    pub fn set_oneshot_rotation(&mut self, rotation: ApiCall<WireguardData>) {
        self.current_call = Some(Call::OneshotKeyRotation(rotation));
    }

    pub fn set_timed_rotation(&mut self, rotation: ApiCall<WireguardData>) {
        self.current_call = Some(Call::TimerKeyRotation(rotation));
    }

    pub fn set_validation(&mut self, validation: ApiCall<Device>) {
        self.current_call = Some(Call::Validation(validation));
    }

    pub fn is_validating(&self) -> bool {
        match &self.current_call {
            Some(Call::Validation(_)) | Some(Call::OneshotKeyRotation(_)) => true,
            _ => false,
        }
    }

    pub fn is_running_timed_totation(&self) -> bool {
        matches!(&self.current_call, Some(Call::TimerKeyRotation(_)))
    }

    pub fn is_idle(&self) -> bool {
        self.current_call.is_none()
    }

    pub fn is_logging_in(&self) -> bool {
        use Call::*;
        match &self.current_call {
            Some(Login(..)) => true,
            _ => false,
        }
    }
}

impl Future for CurrentApiCall {
    type Output = ApiResult;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.current_call.as_mut() {
            Some(call) => {
                let result = Pin::new(call).poll(cx);
                if result.is_ready() {
                    self.current_call = None;
                }
                result
            }
            None => panic!("Polled an unfinished future"),
        }
    }
}

impl FusedFuture for CurrentApiCall {
    fn is_terminated(&self) -> bool {
        self.current_call.is_none()
    }
}

type ApiCall<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>;

enum Call {
    Login(ApiCall<PrivateAccountAndDevice>, Option<ResponseTx<()>>),
    TimerKeyRotation(ApiCall<WireguardData>),
    OneshotKeyRotation(ApiCall<WireguardData>),
    Validation(ApiCall<Device>),
}

impl futures::Future for Call {
    type Output = ApiResult;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use Call::*;
        match &mut *self {
            Login(call, tx) => {
                if let std::task::Poll::Ready(response) = Pin::new(call).poll(cx) {
                    std::task::Poll::Ready(ApiResult::Login(response, tx.take().unwrap()))
                } else {
                    std::task::Poll::Pending
                }
            }
            TimerKeyRotation(call) | OneshotKeyRotation(call) => {
                Pin::new(call).poll(cx).map(ApiResult::Rotation)
            }
            Validation(call) => Pin::new(call).poll(cx).map(ApiResult::Validation),
        }
    }
}

pub(crate) enum ApiResult {
    Login(Result<PrivateAccountAndDevice, Error>, ResponseTx<()>),
    Rotation(Result<WireguardData, Error>),
    Validation(Result<Device, Error>),
}
