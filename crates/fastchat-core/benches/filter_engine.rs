use criterion::{Criterion, criterion_group, criterion_main};
use fastchat_core::{ChatMessage, FilterEngine, GlobalFilterConfig, MessageKind};

fn bench_filter_engine(c: &mut Criterion) {
    let cfg = GlobalFilterConfig {
        include_terms: vec!["hello".into(), "pog".into()],
        exclude_terms: vec!["banword".into()],
        highlight_terms: vec!["rare".into()],
        hidden_users: vec!["spammer".into()],
        min_message_len: 2,
        ..Default::default()
    };
    let engine = FilterEngine::new(cfg);
    let messages: Vec<_> = (0..10_000)
        .map(|i| {
            ChatMessage::new_text(
                "demo",
                format!("user{i}"),
                format!("User{i}"),
                if i % 10 == 0 {
                    "hello rare pog"
                } else {
                    "hello world"
                },
                MessageKind::Chat,
            )
        })
        .collect();

    c.bench_function("filter_engine_eval_10k", |b| {
        b.iter(|| {
            let mut visible = 0usize;
            for msg in &messages {
                if engine.evaluate(msg).visible {
                    visible += 1;
                }
            }
            visible
        })
    });
}

criterion_group!(benches, bench_filter_engine);
criterion_main!(benches);
