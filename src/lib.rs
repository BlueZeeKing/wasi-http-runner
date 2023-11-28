use std::{collections::HashMap, sync::OnceLock, time::Instant};

use ::http::{HeaderMap, HeaderValue, Request, Response};
use http::{IncomingBodyWrapper, Outgoing};
use hyper::body::Incoming;
use io::PollableIndividual;
use wasmtime::{
    component::{bindgen, Component, Linker, Resource},
    AsContext, AsContextMut, Config, Engine, Store,
};

bindgen!();

mod clocks;
mod http;
mod io;

pub struct State {
    errors: HashMap<u32, std::io::Error>,
    fields: HashMap<u32, (bool, HeaderMap<HeaderValue>)>,
    requests: HashMap<u32, Request<hyper::body::Incoming>>,
    responses: HashMap<u32, Response<Outgoing>>,

    incoming: HashMap<u32, IncomingBodyWrapper>,

    pollables: HashMap<u32, Box<dyn PollableIndividual>>,

    full_responses: HashMap<u32, Option<Response<Outgoing>>>,

    current_id: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            errors: HashMap::new(),
            fields: HashMap::new(),
            requests: HashMap::new(),
            responses: HashMap::new(),
            incoming: HashMap::new(),
            pollables: HashMap::new(),
            full_responses: HashMap::new(),
            current_id: 0,
        }
    }
}

impl State {
    pub fn new_id(&mut self) -> u32 {
        self.current_id += 1;
        self.current_id
    }
}

pub async fn service_fn(req: Request<Incoming>) -> anyhow::Result<Response<Outgoing>> {
    tokio::task::spawn_blocking(move || blocking_service(req))
        .await
        .unwrap()
}

fn blocking_service(req: Request<Incoming>) -> anyhow::Result<Response<Outgoing>> {
    let (service, mut store) = instantiate()?;
    let (req_id, res_id) = {
        let state = store.data_mut();

        let req_id = state.new_id();
        let res_id = state.new_id();

        state.requests.insert(req_id, req);
        state.full_responses.insert(res_id, None);

        (req_id, res_id)
    };

    service
        .wasi_http_incoming_handler()
        .call_handle(
            store.as_context_mut(),
            Resource::new_own(req_id),
            Resource::new_own(res_id),
        )
        .unwrap();

    let state = store.data_mut();

    let res = state.full_responses.remove(&res_id).unwrap().unwrap();

    Ok(res)
}

static COMPONENT: OnceLock<(Engine, Component, Linker<State>)> = OnceLock::new();

fn instantiate_lazy() -> wasmtime::Result<(Engine, Component, Linker<State>)> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let component = Component::from_file(&engine, "./component.wasm").unwrap();

    let mut linker = Linker::new(&engine);
    Service::add_to_linker(&mut linker, |state: &mut State| state)?;

    Ok((engine, component, linker))
}

fn instantiate() -> wasmtime::Result<(Service, Store<State>)> {
    let (engine, component, linker) = COMPONENT.get_or_init(|| instantiate_lazy().unwrap());

    let mut store = Store::new(&engine, State::default());

    let (bindings, _) = Service::instantiate(&mut store, &component, &linker)?;

    Ok((bindings, store))
}
