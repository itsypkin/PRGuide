//! The quiet panic hook needs its own process: `Once` runs once per process.

use std::{panic::catch_unwind, sync::Mutex};

use prguide::render::{Charset, MmdfluxRenderer, RenderError, RenderOpts, Renderer};

static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn seen() -> Vec<String> {
    SEEN.lock().unwrap().clone()
}

fn render(src: &str) -> Result<prguide::render::TextDiagram, RenderError> {
    MmdfluxRenderer.render(
        src,
        &RenderOpts {
            charset: Charset::Unicode,
            root_direction: None,
        },
    )
}

const F15: &str = "graph TD\n  subgraph A\n    x\n  end\n  subgraph B\n    x\n  end\n";

/// H1 quiet_hook_silences_only_render_panics
#[test]
fn quiet_hook_silences_only_render_panics() {
    std::panic::set_hook(Box::new(|info| {
        let p = info.payload();
        let text = p
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        SEEN.lock().unwrap().push(text);
    }));
    match render(F15) {
        Err(RenderError::Panic(m)) => assert_eq!(m, "no entry found for key"),
        other => panic!("{other:?}"),
    }
    assert_eq!(seen(), Vec::<String>::new());
    let _ = catch_unwind(|| panic!("outside-render"));
    assert_eq!(seen(), vec!["outside-render".to_owned()]);
    assert!(render("graph LR\n  A --> B\n").is_ok());
    let _ = catch_unwind(|| panic!("after-ok"));
    assert_eq!(
        seen(),
        vec!["outside-render".to_owned(), "after-ok".to_owned()]
    );
    assert!(render(F15).is_err());
    assert_eq!(seen().len(), 2);
}
