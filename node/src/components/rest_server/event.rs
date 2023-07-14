use std::{
    fmt::{self, Display, Formatter},
    mem,
    net::SocketAddr,
};

use derive_more::From;
use static_assertions::const_assert;

use crate::effect::{requests::RestRequest, Responder};

const _REST_EVENT_SIZE: usize = mem::size_of::<Event>();
const_assert!(_REST_EVENT_SIZE < 89);

#[derive(Debug, From)]
pub(crate) enum Event {
    Initialize,
    /// The background task running the HTTP server has finished binding its port.
    BindComplete(SocketAddr),
    #[from]
    RestRequest(RestRequest),
    GetMetricsResult {
        text: Option<String>,
        main_responder: Responder<Option<String>>,
    },
}

impl Display for Event {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        match self {
            Event::Initialize => write!(formatter, "initialize"),
            Event::BindComplete(local_addr) => write!(formatter, "bind complete: {}", local_addr),
            Event::RestRequest(request) => write!(formatter, "{}", request),
            Event::GetMetricsResult { text, .. } => match text {
                Some(txt) => write!(formatter, "get metrics ({} bytes)", txt.len()),
                None => write!(formatter, "get metrics (failed)"),
            },
        }
    }
}
