use std::{collections::HashMap, sync::{Arc, Mutex}};

pub mod track;

#[derive(Debug)]
pub struct State {
    pub tracks: HashMap<usize, Arc<Mutex<track::Track>>>,
}

impl State {
    pub fn new() -> Self {
        State { tracks: HashMap::new() }
    }
}
