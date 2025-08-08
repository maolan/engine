pub mod track;

#[derive(Debug)]
pub struct State {
    pub tracks: Vec<track::Track>,
}

impl State {
    pub fn new() -> Self {
        State { tracks: vec![] }
    }
}
