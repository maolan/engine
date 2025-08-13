pub mod track;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct State {
    pub tracks: Vec<Arc<Mutex<track::Track>>>,
}

impl State {
    pub fn new() -> Self {
        State { tracks: vec![] }
    }
}
