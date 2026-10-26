mod camera;
mod dashboard;
mod events;
mod http;
mod icons;
mod js;
mod live;
mod push;
mod settings;

use bambuspa_api as api;
use dioxus::prelude::*;

use crate::camera::Camera;

fn main() {
    dioxus::launch(App);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Screen {
    Main,
    Events,
    Settings,
}

#[derive(Clone, Copy)]
pub struct State {
    pub screen: Signal<Screen>,
    pub log: Signal<String>,
    pub printers: Signal<Vec<api::PrinterSummary>>,
    pub current: Signal<Option<i64>>,
    pub status: Signal<Option<api::Status>>,
    pub reachable: Signal<bool>,
    pub timelapses: Signal<Vec<api::Timelapse>>,
    pub dashboard: Signal<Vec<String>>,
    pub camera: Camera,
}

impl State {
    fn new() -> State {
        let current = js::query_param("printer")
            .or_else(|| js::stored("printer"))
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id != 0);
        State {
            screen: Signal::new(Screen::Main),
            log: Signal::new(String::new()),
            printers: Signal::new(Vec::new()),
            current: Signal::new(current),
            status: Signal::new(None),
            reachable: Signal::new(true),
            timelapses: Signal::new(Vec::new()),
            dashboard: Signal::new(
                dashboard::SECTIONS
                    .iter()
                    .map(|s| s.0.to_string())
                    .collect(),
            ),
            camera: Camera::new(),
        }
    }

    pub fn with_printer(&self, url: &str) -> String {
        match *self.current.peek() {
            Some(id) => {
                let join = if url.contains('?') { '&' } else { '?' };
                format!("{url}{join}printer={id}")
            }
            None => url.to_string(),
        }
    }
}

pub fn send_live(msg: api::ClientMessage) {
    consume_context::<Coroutine<api::ClientMessage>>().send(msg);
}

pub fn show(mut state: State, screen: Screen) {
    state.screen.set(screen);
    state.log.set(String::new());
    js::window().scroll_to_with_x_and_y(0.0, 0.0);
}

pub fn select_printer(mut state: State, id: i64) {
    if *state.current.peek() == Some(id) {
        return;
    }
    state.current.set(Some(id));
    js::store("printer", &id.to_string());
    camera::close_player(state);
    send_live(api::ClientMessage::Select { printer: id });
}

pub fn apply_printer_list(mut state: State, list: Vec<api::PrinterSummary>) {
    let current = *state.current.peek();
    if !list.iter().any(|p| Some(p.id) == current) {
        state.current.set(list.first().map(|p| p.id));
    }
    state.printers.set(list);
}

#[component]
fn App() -> Element {
    let state = use_context_provider(State::new);
    use_coroutine(move |rx| live::run(rx, state));
    let push = use_context_provider(push::Push::new);
    use_hook(move || {
        spawn(push::start(push));
        spawn(dashboard::load(state));
    });
    let page = match state.screen.cloned() {
        Screen::Main => rsx! { dashboard::MainScreen {} },
        Screen::Events => rsx! { events::EventsScreen {} },
        Screen::Settings => rsx! { settings::SettingsScreen {} },
    };
    let log = state.log.cloned();
    rsx! {
        {page}
        div { id: "log", "{log}" }
    }
}
