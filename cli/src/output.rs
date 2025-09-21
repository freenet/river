use serde::Serialize;
use std::fmt::Display;
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Human,
    Json,
}

impl FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "human" => Ok(OutputFormat::Human),
            "json" => Ok(OutputFormat::Json),
            _ => Err(format!("Unknown output format: {}", s)),
        }
    }
}

impl Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Human => write!(f, "human"),
            OutputFormat::Json => write!(f, "json"),
        }
    }
}

#[allow(dead_code)]
pub fn print_output<T: Serialize + Display>(data: T, format: OutputFormat) {
    match format {
        OutputFormat::Human => println!("{}", data),
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&data).unwrap());
        }
    }
}
