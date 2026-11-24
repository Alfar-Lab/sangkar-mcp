pub struct Index {
    pub name: String,
}

impl Index {
    pub fn new(name: &str) -> Self {
        Index {
            name: name.to_string(),
        }
    }

    pub fn lookup(&self, key: usize) -> usize {
        normalize(key) + 1
    }
}

pub fn normalize(key: usize) -> usize {
    key % 8
}

pub fn load(key: usize) -> usize {
    let index = Index::new("main");
    index.lookup(normalize(key))
}
