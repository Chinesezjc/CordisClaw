use expr::evaluate_expression;
use serde::{Deserialize, Serialize};
use std::io::{self, Read};

#[derive(Debug, Deserialize)]
struct ExprRequest {
    expression: String,
}

#[derive(Debug, Serialize)]
struct ExprResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() {
    let mut input = String::new();
    if let Err(err) = io::stdin().read_to_string(&mut input) {
        eprintln!("read stdin failed: {err}");
        std::process::exit(1);
    }

    let payload = match serde_json::from_str::<ExprRequest>(&input) {
        Ok(request) => match evaluate_expression(&request.expression) {
            Ok(value) => ExprResponse {
                value: Some(value),
                error: None,
            },
            Err(err) => ExprResponse {
                value: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => ExprResponse {
            value: None,
            error: Some(format!("invalid request: {err}")),
        },
    };

    match serde_json::to_string(&payload) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("serialize response failed: {err}");
            std::process::exit(1);
        }
    }
}
