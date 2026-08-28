use criterion::{criterion_group, criterion_main, Criterion};
use raven::agent::ChatMessage;
use raven::tokenizer::{count_tokens, history_tokens, message_tokens};

fn prose_sample() -> String {
    "The quick brown fox jumps over the lazy dog. ".repeat(2000)
}

fn code_sample() -> String {
    let line = "fn process(items: &[Item]) -> Result<Vec<Output>, Error> { items.iter().map(|i| i.transform()).collect() }\n";
    line.repeat(1500)
}

fn json_sample() -> String {
    let obj = r#"{"path": "src/main.rs", "content": "fn main() { println!(\"hello\"); }", "mode": "overwrite"}"#;
    obj.repeat(800)
}

fn history_sample() -> Vec<ChatMessage> {
    let mut msgs = Vec::new();
    msgs.push(ChatMessage {
        role: "system".into(),
        content: Some("You are a coding agent.".into()),
        tool_calls: None,
        tool_call_id: None,
        usage: None,
    });
    for i in 0..200 {
        msgs.push(ChatMessage {
            role: "user".into(),
            content: Some(format!("Please fix the bug in module {i} and add tests.")),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        });
        msgs.push(ChatMessage {
            role: "assistant".into(),
            content: Some(format!("I found the issue in module {i} and fixed it.")),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        });
    }
    msgs
}

fn bench(c: &mut Criterion) {
    let prose = prose_sample();
    let code = code_sample();
    let json = json_sample();
    let history = history_sample();

    let mut g = c.benchmark_group("tokenizer");
    g.throughput(criterion::Throughput::Bytes(prose.len() as u64));
    g.bench_function("count_tokens_prose_50k", |b| {
        b.iter(|| count_tokens(std::hint::black_box(&prose)))
    });
    g.throughput(criterion::Throughput::Bytes(code.len() as u64));
    g.bench_function("count_tokens_code_100k", |b| {
        b.iter(|| count_tokens(std::hint::black_box(&code)))
    });
    g.throughput(criterion::Throughput::Bytes(json.len() as u64));
    g.bench_function("count_tokens_json_50k", |b| {
        b.iter(|| count_tokens(std::hint::black_box(&json)))
    });
    g.bench_function("history_tokens_400_msgs", |b| {
        b.iter(|| history_tokens(std::hint::black_box(&history)))
    });
    g.bench_function("message_tokens_single", |b| {
        b.iter(|| message_tokens(std::hint::black_box(&history[1])))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
