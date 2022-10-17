use serde::{Deserialize, Serialize};
pub mod protos;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HelloResponse {
    pub name: String
}

