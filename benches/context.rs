use criterion::{criterion_group, criterion_main, Criterion};
use raven::agent::ChatMessage;
use raven::context::compact_if_needed_llm;

fn build_history(n: usize, tool_heavy: bool) -> Vec<ChatMessage> {
    let mut msgs = Vec::new();
    msgs.push(ChatMessage {
        role: "system".into(),
        content: Some("You are a coding agent.".into()),
        tool_calls: None,
        tool_call_id: None,
        usage: None,
    });
    for i in 0..n {
        msgs.push(ChatMessage {
            role: "user".into(),
            content: Some(format!("Task number {i}: refactor the auth module.")),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        });
        if tool_heavy {
            msgs.push(ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![raven::agent::ToolCall {
                    id: format!("call_{i}"),
                    type_: "function".into(),
                    function: raven::agent::FunctionCall {
                        name: "read_file".into(),
                        arguments: r#"{"path":"src/auth.rs"}"#.into(),
                    },
                }]),
                tool_call_id: None,
                usage: None,
            });
            msgs.push(ChatMessage {
                role: "tool".into(),
                content: Some("x".repeat(6000)),
                tool_calls: None,
                tool_call_id: Some(format!("call_{i}")),
                usage: None,
            });
        } else {
            msgs.push(ChatMessage {
                role: "assistant".into(),
                content: Some(format!("I refactored module {i}.")),
                tool_calls: None,
                tool_call_id: None,
                usage: None,
            });
        }
    }
    msgs
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut g = c.benchmark_group("context");

    g.bench_function("compact_plain_200_turns", |b| {
        let msgs = build_history(200, false);
        b.iter_batched(
            || msgs.clone(),
            |mut m| {
                rt.block_on(compact_if_needed_llm(&mut m, 8192, 0.1, None, |_| {
                    Box::pin(async { None })
                }))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    g.bench_function("compact_tool_heavy_100_turns", |b| {
        let msgs = build_history(100, true);
        b.iter_batched(
            || msgs.clone(),
            |mut m| {
                rt.block_on(compact_if_needed_llm(&mut m, 8192, 0.1, None, |_| {
                    Box::pin(async { None })
                }))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    g.bench_function("compact_noop_under_threshold", |b| {
        let msgs = build_history(20, false);
        b.iter_batched(
            || msgs.clone(),
            |mut m| {
                rt.block_on(compact_if_needed_llm(&mut m, 128_000, 0.75, None, |_| {
                    Box::pin(async { None })
                }))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
