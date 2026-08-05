//! 生成 `docs/agent/interfaces.json`。
//!
//! 该文件必须与 dylib 内嵌 docs 逐字段一致（loader 会交叉校验），因此由
//! `docs_value()` 单一来源导出而不是手写：
//!
//! ```sh
//! cargo run -p llm_openai --example dump_docs > \
//!   fixtures/plugins/llm_openai/docs/agent/interfaces.json
//! ```
fn main() {
    let docs = llm_openai::__test_docs();
    println!(
        "{}",
        serde_json::to_string_pretty(&docs).expect("serialize docs")
    );
}
