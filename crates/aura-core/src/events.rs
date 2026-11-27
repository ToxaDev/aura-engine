//! How the engine tells its front about something that happened. The desktop
//! app hands the event to its windows, the streamer to its web clients.

use std::sync::Arc;

/// Where events go. Cheap to clone; a long job keeps one for as long as it runs.
#[derive(Clone)]
pub struct Events(Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>);

impl Events {
    pub fn new(sink: impl Fn(&str, serde_json::Value) + Send + Sync + 'static) -> Self {
        Events(Arc::new(sink))
    }

    pub fn emit(&self, name: &str, payload: serde_json::Value) {
        (self.0)(name, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn an_event_reaches_the_sink_with_its_name_and_payload() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events = {
            let seen = seen.clone();
            Events::new(move |name, v| seen.lock().unwrap().push((name.to_string(), v)))
        };
        events.clone().emit("a:b", serde_json::json!({ "n": 1 }));
        assert_eq!(*seen.lock().unwrap(), vec![("a:b".to_string(), serde_json::json!({ "n": 1 }))]);
    }
}
