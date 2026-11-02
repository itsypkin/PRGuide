//! The quiet panic hook needs its own process: `Once` runs once per process.

use std::{panic::catch_unwind, sync::Mutex};

use prguide::render::{Charset, Limit, MmdfluxRenderer, RenderError, RenderOpts, Renderer};

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
    let before = seen().len();
    assert!(matches!(
        render("graph TD\n  A --> \n  ]]] [[\n"),
        Err(RenderError::Invalid(_))
    ));
    let _ = catch_unwind(|| panic!("after-invalid"));
    assert_eq!(seen().len(), before + 1);
    assert_eq!(seen().last().map(String::as_str), Some("after-invalid"));
    assert!(matches!(
        render("pie title X\n  \"a\" : 1\n"),
        Err(RenderError::Unsupported(_))
    ));
    let _ = catch_unwind(|| panic!("after-unsupported"));
    assert_eq!(seen().len(), before + 2);
    assert_eq!(seen().last().map(String::as_str), Some("after-unsupported"));
    // A pipeline-level TooLarge (the guard exists by then).
    let long_title = format!("sequenceDiagram\n  title {}\n  A->>B: hi\n", "t".repeat(61));
    assert!(matches!(
        render(&long_title),
        Err(RenderError::TooLarge {
            limit: Limit::LabelChars,
            actual: 61,
            max: 60
        })
    ));
    let _ = catch_unwind(|| panic!("after-too-large"));
    assert_eq!(
        seen(),
        [
            "outside-render",
            "after-ok",
            "after-invalid",
            "after-unsupported",
            "after-too-large"
        ]
    );
}
