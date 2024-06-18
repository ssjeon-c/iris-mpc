use crate::setup::iris_db::iris::IrisCodeArray;
use base64::{engine::general_purpose, Engine};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct SQSMessage {
    #[serde(rename = "Type")]
    pub notification_type: String,
    #[serde(rename = "MessageId")]
    pub message_id:        String,
    #[serde(rename = "SequenceNumber")]
    pub sequence_number:   String,
    #[serde(rename = "TopicArn")]
    pub topic_arn:         String,
    #[serde(rename = "Message")]
    pub message:           String,
    #[serde(rename = "Timestamp")]
    pub timestamp:         String,
    #[serde(rename = "UnsubscribeURL")]
    pub unsubscribe_url:   String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SMPCRequest {
    pub request_type: String,
    pub request_id:   String,
    pub iris_code:    String,
    pub mask_code:    String,
}

impl SMPCRequest {
    fn decode_bytes(bytes: &[u8]) -> [u16; IrisCodeArray::IRIS_CODE_SIZE] {
        let code = general_purpose::STANDARD.decode(bytes).unwrap();
        let mut buffer = [0u16; IrisCodeArray::IRIS_CODE_SIZE];
        buffer.copy_from_slice(bytemuck::cast_slice(&code));
        buffer
    }
    pub fn get_iris_shares(&self) -> [u16; IrisCodeArray::IRIS_CODE_SIZE] {
        Self::decode_bytes(self.iris_code.as_bytes())
    }
    pub fn get_mask_shares(&self) -> [u16; IrisCodeArray::IRIS_CODE_SIZE] {
        Self::decode_bytes(self.mask_code.as_bytes())
    }
}