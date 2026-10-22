//! `tool_calls` 链整形：把 `messages` 修成 DeepSeek/OpenAI 都能接受的形式。
//!
//! 供应商要求两条方向相反的紧邻性：
//!
//! 1. 带 `tool_calls` 的 assistant 消息，其**每一个** call 都必须被紧随其后的
//!    `role=="tool"` 消息应答（没被应答的 call 会让请求非法）；
//! 2. 任何 `role=="tool"` 消息都必须是对某条**紧邻在前**的 assistant
//!    `tool_calls` 的应答（孤儿 tool 消息同样让请求非法）。
//!
//! 违反任一条，DeepSeek 会整轮 400：
//! `Messages with role 'tool' must be a response to a preceding message with 'tool_calls'`。
//! agent 循环里一旦有别的消息（尤其是失败提示的 user 消息）插进这条链，就会
//! 同时违反两条：assistant 的 call 掉队，tool 结果也变成孤儿。
//!
//! 本模块只做纯函数整形，不碰 I/O，两个方向各扫一遍，且两次扫描都收敛到
//! 不动点——对已整形过的数组再跑一次返回全 0（幂等）。

use serde_json::Value;
use std::collections::BTreeSet;

/// 一次整形丢掉了什么。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RepairStats {
    /// 从 assistant 的 `tool_calls` 数组里删掉的 call 数。
    pub dropped_calls: usize,
    /// 删掉的孤儿 `role=="tool"` 消息数。
    pub dropped_tool_messages: usize,
    /// 一个 call 都留不下而整条删除的 assistant 消息数。
    pub dropped_assistant_messages: usize,
}

impl RepairStats {
    /// 有任何改动即为真——调用方据此决定是否打诊断。
    pub fn changed(&self) -> bool {
        self.dropped_calls + self.dropped_tool_messages + self.dropped_assistant_messages > 0
    }
}

/// 就地整形 `messages`，返回丢掉了什么。
///
/// 幂等：对返回 `changed() == false` 的数组再跑一次仍返回全 0。
pub(crate) fn sanitize_tool_chains(messages: &mut Vec<Value>) -> RepairStats {
    let mut stats = RepairStats::default();

    // 第一遍：assistant 的 tool_calls 只保留「紧随其后连续 tool 消息」里出现过
    // 的 id。遇到第一个非 tool 消息就停——只有这样，被 user 消息隔断的链才会
    // 被抓出来，而不是把隔断之后的 tool 结果也算成"已应答"。
    let mut remove_assistants: Vec<usize> = Vec::new();
    let mut filtered_calls: Vec<(usize, Vec<Value>)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(calls) = tool_calls_of(message) else {
            continue;
        };
        if calls.is_empty() {
            // 没有 call 就没有链可修：别把这种（可能带正文的）消息整条删掉。
            continue;
        }
        let mut answered: BTreeSet<String> = BTreeSet::new();
        for next in messages.iter().skip(index + 1) {
            if role_of(next) != Some("tool") {
                break;
            }
            if let Some(id) = next.get("tool_call_id").and_then(Value::as_str) {
                answered.insert(id.to_string());
            }
        }
        let kept: Vec<Value> = calls
            .iter()
            .filter(|call| is_kept(call, &answered))
            .cloned()
            .collect();
        if kept.len() == calls.len() {
            continue;
        }
        if kept.is_empty() {
            remove_assistants.push(index);
        } else {
            stats.dropped_calls += calls.len() - kept.len();
            filtered_calls.push((index, kept));
        }
    }
    // 先按原下标过滤（不受删除影响），再按逆序删除整条消息。
    for (index, kept) in filtered_calls {
        // `for … in option.into_iter()` 而不是 `if let Some`：后者会在这一行
        // 留下一个永远不会执行的隐式 else，把整行拖成零计数。
        for object in messages[index].as_object_mut().into_iter() {
            object.insert("tool_calls".to_string(), Value::Array(kept.clone()));
        }
    }
    for index in remove_assistants.into_iter().rev() {
        messages.remove(index);
        stats.dropped_assistant_messages += 1;
    }

    // 第二遍：任何 tool 消息都必须对得上「当前生效的」assistant 链——最近一条
    // assistant 的 tool_calls；遇到新的 assistant 就换掉，遇到 user 就清空。
    let mut active: BTreeSet<String> = BTreeSet::new();
    let mut remove_tools: Vec<usize> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match role_of(message) {
            Some("assistant") => {
                active = tool_calls_of(message).map(call_ids).unwrap_or_default();
            }
            Some("user") => active.clear(),
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !active.contains(id) {
                    remove_tools.push(index);
                }
            }
            _ => {}
        }
    }
    for index in remove_tools.into_iter().rev() {
        messages.remove(index);
        stats.dropped_tool_messages += 1;
    }

    stats
}

/// `messages[i]["role"]`；非字符串或没有该键都当作「不认识的 role」。
fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// `messages[i]["tool_calls"]` 的数组视图；非数组一律返回 `None`（跳过整形）。
fn tool_calls_of(message: &Value) -> Option<&[Value]> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.as_slice())
}

/// 一条 assistant 的 tool_calls 里所有带 id 的 call id。
fn call_ids(calls: &[Value]) -> BTreeSet<String> {
    calls
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// 该 call 是否留在 assistant 里：有 id 就必须被应答过；没有 id 的畸形元素
/// 认不出对应关系，照旧跳过（不因此在 count 里记账，也不因此触发整形）。
fn is_kept(call: &Value, answered: &BTreeSet<String>) -> bool {
    match call.get("id").and_then(Value::as_str) {
        Some(id) => answered.contains(id),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> Value {
        json!({"id": id, "type": "function", "function": {"name": "tool", "arguments": "{}"}})
    }

    fn idless_call() -> Value {
        json!({"function": {"name": "tool", "arguments": "{}"}})
    }

    fn assistant_with(calls: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": "", "tool_calls": calls})
    }

    fn tool_result(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": "ok"})
    }

    fn call_ids_in(message: &Value) -> Vec<String> {
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| call.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn sanitized(mut messages: Vec<Value>) -> (Vec<Value>, RepairStats) {
        let stats = sanitize_tool_chains(&mut messages);
        (messages, stats)
    }

    fn tool_message_count(messages: &[Value]) -> usize {
        messages
            .iter()
            .filter(|m| role_of(m) == Some("tool"))
            .count()
    }

    // 1. 合法链原样不动。
    #[test]
    fn legal_chain_is_left_untouched() {
        let before = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "u"}),
            assistant_with(vec![call("a")]),
            tool_result("a"),
            json!({"role": "assistant", "content": "done"}),
        ];
        let (after, stats) = sanitized(before.clone());
        assert_eq!(after, before);
        assert!(!stats.changed());
        assert_eq!(stats, RepairStats::default());
    }

    // 2. 部分应答：只留被应答的 call，tool 结果保留。
    #[test]
    fn unanswered_call_is_filtered_out() {
        let (after, stats) = sanitized(vec![
            assistant_with(vec![call("a"), call("b")]),
            tool_result("a"),
        ]);
        assert_eq!(stats.dropped_calls, 1);
        assert_eq!(stats.dropped_tool_messages, 0);
        assert_eq!(stats.dropped_assistant_messages, 0);
        assert!(stats.changed());
        assert_eq!(after.len(), 2);
        assert_eq!(call_ids_in(&after[0]), vec!["a".to_string()]);
        assert_eq!(after[1], tool_result("a"));
    }

    // 3. 线上现场：assistant[A,B] / tool(A) / user(x) / tool(B)。
    #[test]
    fn user_message_between_tool_results_drops_the_orphan() {
        let (after, stats) = sanitized(vec![
            assistant_with(vec![call("a"), call("b")]),
            tool_result("a"),
            json!({"role": "user", "content": "🔴 TOOL 'a' FAILED: boom"}),
            tool_result("b"),
        ]);
        assert!(stats.changed());
        assert_eq!(stats.dropped_calls, 1);
        assert_eq!(stats.dropped_tool_messages, 1);
        assert_eq!(stats.dropped_assistant_messages, 0);
        assert_eq!(after.len(), 3);
        assert_eq!(call_ids_in(&after[0]), vec!["a".to_string()]);
        assert_eq!(after[1], tool_result("a"));
        assert_eq!(after[2]["role"], json!("user"));
        // 没有孤儿 tool 消息留下：只剩 tool(A) 一条。
        assert_eq!(tool_message_count(&after), 1);
    }

    // 4. 一个 call 都没有应答 → assistant 整条删除。
    #[test]
    fn assistant_without_any_tool_result_is_dropped() {
        let (after, stats) = sanitized(vec![
            json!({"role": "user", "content": "u"}),
            assistant_with(vec![call("a")]),
            json!({"role": "user", "content": "again"}),
        ]);
        assert_eq!(stats.dropped_assistant_messages, 1);
        assert_eq!(stats.dropped_calls, 0);
        assert_eq!(stats.dropped_tool_messages, 0);
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|m| role_of(m) != Some("assistant")));
    }

    // 5. 空数组 / 只有 system+user → 全 0。
    #[test]
    fn empty_and_plain_conversations_are_untouched() {
        let (after, stats) = sanitized(Vec::new());
        assert!(after.is_empty());
        assert_eq!(stats, RepairStats::default());
        let plain = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "u"}),
        ];
        let (after, stats) = sanitized(plain.clone());
        assert_eq!(after, plain);
        assert!(!stats.changed());
    }

    // 6. 两条连续 assistant 链各自独立处理；顺带覆盖「无 tool_calls 的
    //    assistant」与「非数组 tool_calls」两条跳过臂。
    #[test]
    fn consecutive_assistant_chains_are_repaired_independently() {
        let (after, stats) = sanitized(vec![
            json!({"role": "system", "content": "s"}),
            assistant_with(vec![call("a")]),
            tool_result("a"),
            json!({"role": "assistant", "content": "", "tool_calls": "not an array"}),
            json!({"role": "assistant", "content": "", "tool_calls": []}),
            assistant_with(vec![call("b"), call("c")]),
            tool_result("b"),
            json!({"role": "assistant", "content": "final"}),
        ]);
        assert!(stats.changed());
        assert_eq!(stats.dropped_calls, 1);
        assert_eq!(stats.dropped_tool_messages, 0);
        assert_eq!(stats.dropped_assistant_messages, 0);
        assert_eq!(after.len(), 8);
        assert_eq!(call_ids_in(&after[1]), vec!["a".to_string()]);
        assert_eq!(call_ids_in(&after[5]), vec!["b".to_string()]);
        assert_eq!(after[3]["tool_calls"], json!("not an array"));
        assert_eq!(after[4]["tool_calls"], json!([]));
    }

    // 7. 幂等：第二次调用全 0。
    #[test]
    fn sanitize_is_idempotent() {
        let mut messages = vec![
            assistant_with(vec![call("a"), call("b")]),
            tool_result("a"),
            json!({"role": "user", "content": "🔴 TOOL 'a' FAILED: boom"}),
            tool_result("b"),
            assistant_with(vec![call("c")]),
        ];
        let first = sanitize_tool_chains(&mut messages);
        assert!(first.changed());
        let second = sanitize_tool_chains(&mut messages);
        assert_eq!(second, RepairStats::default());
        assert!(!second.changed());
        assert_eq!(call_ids_in(&messages[0]), vec!["a".to_string()]);
        assert_eq!(messages.len(), 3);
    }

    // 8. 畸形元素：没有 id 的 call 照旧留下、没有 tool_call_id 的 tool 消息被删、
    //    不认识的 role 忽略。
    #[test]
    fn malformed_tool_calls_and_tool_results_are_skipped() {
        let (after, stats) = sanitized(vec![
            json!({"content": "no role at all"}),
            assistant_with(vec![call("a"), idless_call()]),
            json!({"role": "tool", "content": "no tool_call_id"}),
        ]);
        assert_eq!(stats.dropped_calls, 1);
        assert_eq!(stats.dropped_tool_messages, 1);
        assert_eq!(stats.dropped_assistant_messages, 0);
        assert_eq!(after.len(), 2);
        assert_eq!(call_ids_in(&after[1]), Vec::<String>::new());
        assert_eq!(after[1]["tool_calls"].as_array().map(Vec::len), Some(1));
    }
}
