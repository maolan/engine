#[derive(Debug)]
pub struct Track {
    buffer: Vec<f32>,
}

impl Track {
    pub fn new() -> Self {
        Track {buffer: vec![]}
    }

    pub fn process(&mut self) {
        self.buffer.clear();
    }
}
