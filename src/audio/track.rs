#[derive(Debug)]
pub struct Track {
    id: usize,
    buffer: Vec<f32>,
}

impl Track {
    pub fn new(id: usize) -> Self {
        Track {
            id,
            buffer: vec![],
        }
    }

    pub fn process(&mut self) {
        self.buffer.clear();
    }

    pub fn id(&self) -> usize {
        self.id
    }
}
