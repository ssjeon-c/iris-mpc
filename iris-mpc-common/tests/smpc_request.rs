mod tests {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use http::StatusCode;
    use iris_mpc_common::helpers::{
        key_pair::{SharesDecodingError, SharesEncryptionKeyPairs},
        sha256::calculate_sha256,
        smpc_request::{IrisCodesJSON, UniquenessRequest},
    };
    use serde_json::json;
    use sodiumoxide::crypto::{box_::PublicKey, sealedbox};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    const PREVIOUS_PUBLIC_KEY: &str = "1UY8lKlS7aVj5ZnorSfLIHlG3jg+L4ToVi4K+mLKqFQ=";
    const PREVIOUS_PRIVATE_KEY: &str = "X26wWfzP5fKMP7QMz0X3eZsEeF4NhJU92jT69wZg6x8=";

    const CURRENT_PUBLIC_KEY: &str = "HDp962tQyZIG9t+GX4JM0i1wgJx/YGpHGsuDSD34KBA=";
    const CURRENT_PRIVATE_KEY: &str = "14Z6Zijg3kbFN//R9BRKLeTS/wCiZMfK6AurEr/nAZg=";

    fn get_key_pairs(
        current_pk_string: String,
        previous_pk_string: String,
    ) -> SharesEncryptionKeyPairs {
        SharesEncryptionKeyPairs::from_b64_private_key_strings(
            current_pk_string.to_string().clone(),
            previous_pk_string.to_string().clone(),
        )
        .unwrap()
    }

    fn mock_iris_codes_json() -> IrisCodesJSON {
        IrisCodesJSON {
            iris_version:           "1.0".to_string(),
            iris_shares_version:    "1.3".to_string(),
            left_iris_code_shares:  STANDARD.encode("left_iris_code_mock"),
            right_iris_code_shares: STANDARD.encode("right_iris_code_mock"),
            left_mask_code_shares:  STANDARD.encode("left_iris_mask_mock"),
            right_mask_code_shares: STANDARD.encode("right_iris_mask_mock"),
        }
    }

    fn get_mock_smpc_request_with_hashes(hashes: [String; 3]) -> UniquenessRequest {
        UniquenessRequest {
            batch_size:              Some(1),
            signup_id:               "signup_mock".to_string(),
            s3_presigned_url:        "https://example.com/mock".to_string(),
            iris_shares_file_hashes: hashes,
        }
    }

    fn get_mock_request() -> UniquenessRequest {
        UniquenessRequest {
            batch_size:              None,
            signup_id:               "test_signup_id".to_string(),
            s3_presigned_url:        "https://example.com/package".to_string(),
            iris_shares_file_hashes: [
                "hash_0".to_string(),
                "hash_1".to_string(),
                "hash_2".to_string(),
            ],
        }
    }

    #[tokio::test]
    async fn test_retrieve_iris_shares_from_s3_success() {
        let mock_server = MockServer::start().await;

        // Simulate a successful response from the presigned URL
        let response_body = json!({
            "iris_share_0": "share_0_data",
            "iris_share_1": "share_1_data",
            "iris_share_2": "share_2_data"
        });

        let template = ResponseTemplate::new(StatusCode::OK).set_body_json(response_body.clone());

        Mock::given(method("GET"))
            .and(path("/test_presign_url"))
            .respond_with(template)
            .mount(&mock_server)
            .await;

        let smpc_request = UniquenessRequest {
            batch_size:              None,
            signup_id:               "test_signup_id".to_string(),
            s3_presigned_url:        mock_server.uri().clone() + "/test_presign_url",
            iris_shares_file_hashes: [
                "hash_0".to_string(),
                "hash_1".to_string(),
                "hash_2".to_string(),
            ],
        };

        let result = smpc_request.get_iris_data_by_party_id(0).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "share_0_data".to_string());
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_success() {
        // Mocked base64 encoded JSON string
        let iris_codes_json = IrisCodesJSON {
            iris_version:           "1.0".to_string(),
            iris_shares_version:    "1.3".to_string(),
            left_iris_code_shares:  "left_code".to_string(),
            right_iris_code_shares: "right_code".to_string(),
            left_mask_code_shares:  "left_mask".to_string(),
            right_mask_code_shares: "right_mask".to_string(),
        };

        let decoded_public_key = STANDARD.decode(CURRENT_PUBLIC_KEY.as_bytes()).unwrap();
        let shares_encryption_public_key = PublicKey::from_slice(&decoded_public_key).unwrap();

        // convert iris code to JSON string, sealbox and encode as BASE64
        let json_string = serde_json::to_string(&iris_codes_json).unwrap();
        let sealed_box = sealedbox::seal(json_string.as_bytes(), &shares_encryption_public_key);
        let encoded_share = STANDARD.encode(sealed_box);

        let smpc_request = get_mock_request();
        let key_pair = get_key_pairs(
            PREVIOUS_PRIVATE_KEY.to_string(),
            CURRENT_PRIVATE_KEY.to_string(),
        );

        let result = smpc_request.decrypt_iris_share(encoded_share, key_pair);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), iris_codes_json);
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_using_previous_valid_key() {
        // Mocked base64 encoded JSON string
        let iris_codes_json = mock_iris_codes_json();

        // Use previous public key to encrypt the shares
        let decoded_public_key = STANDARD.decode(PREVIOUS_PUBLIC_KEY.as_bytes()).unwrap();
        let shares_encryption_public_key = PublicKey::from_slice(&decoded_public_key).unwrap();

        // convert iris code to JSON string, sealbox and encode as BASE64
        let json_string = serde_json::to_string(&iris_codes_json).unwrap();
        let sealed_box = sealedbox::seal(json_string.as_bytes(), &shares_encryption_public_key);
        let encoded_share = STANDARD.encode(sealed_box);

        let smpc_request = get_mock_request();
        let key_pair = get_key_pairs(
            PREVIOUS_PRIVATE_KEY.to_string(),
            CURRENT_PRIVATE_KEY.to_string(),
        );

        // Decrypt the share. It will succeed, by first attempting to use the current
        // private key (failing), and then the previous private key (succeeding)
        let result = smpc_request.decrypt_iris_share(encoded_share, key_pair);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), iris_codes_json);
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_non_existent_previous_private_key() {
        // Mocked base64 encoded JSON string
        let iris_codes_json = mock_iris_codes_json();

        // Use previous public key to encrypt the shares
        let decoded_public_key = STANDARD.decode(PREVIOUS_PUBLIC_KEY.as_bytes()).unwrap();
        let shares_encryption_public_key = PublicKey::from_slice(&decoded_public_key).unwrap();
        let json_string = serde_json::to_string(&iris_codes_json).unwrap();
        let sealed_box = sealedbox::seal(json_string.as_bytes(), &shares_encryption_public_key);
        let encoded_share = STANDARD.encode(&sealed_box);

        // Set the previous key to be empty
        let key_pair = get_key_pairs(CURRENT_PRIVATE_KEY.to_string(), "".to_string());

        let smpc_request = get_mock_request();

        // Decrypt the share. It will fail: it will attempt to decrypt using the current
        // key, but the share was encrypted using the current key. The previous
        // key does not exist, so it will return a sealed box open error
        let result = smpc_request.decrypt_iris_share(encoded_share, key_pair);
        assert!(matches!(
            result,
            Err(SharesDecodingError::SealedBoxOpenError)
        ));
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_invalid_base64() {
        let invalid_base64 = "InvalidBase64String";
        let key_pair = get_key_pairs(
            CURRENT_PRIVATE_KEY.to_string(),
            CURRENT_PRIVATE_KEY.to_string(),
        );
        let smpc_request = get_mock_request();

        let result = smpc_request.decrypt_iris_share(invalid_base64.to_string(), key_pair);

        assert!(matches!(
            result,
            Err(SharesDecodingError::Base64DecodeError)
        ));
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_invalid_utf8() {
        let invalid_utf8 = vec![0, 159, 146, 150]; // Not valid UTF-8

        let decoded_public_key = STANDARD.decode(CURRENT_PUBLIC_KEY.as_bytes()).unwrap();
        let shares_encryption_public_key = PublicKey::from_slice(&decoded_public_key).unwrap();
        let sealed_box = sealedbox::seal(&invalid_utf8, &shares_encryption_public_key);
        let encoded_share = STANDARD.encode(&sealed_box);

        let key_pair = get_key_pairs(
            PREVIOUS_PRIVATE_KEY.to_string(),
            CURRENT_PRIVATE_KEY.to_string(),
        );
        let smpc_request = get_mock_request();

        let result = smpc_request.decrypt_iris_share(encoded_share, key_pair);

        assert!(matches!(
            result,
            Err(SharesDecodingError::DecodedShareParsingToUTF8Error(_))
        ));
    }

    #[tokio::test]
    async fn test_decrypt_iris_share_invalid_json() {
        let invalid_json = "totally-not-a-json-string";

        let decoded_public_key = STANDARD.decode(CURRENT_PUBLIC_KEY.as_bytes()).unwrap();
        let shares_encryption_public_key = PublicKey::from_slice(&decoded_public_key).unwrap();
        let sealed_box = sealedbox::seal(invalid_json.as_bytes(), &shares_encryption_public_key);
        let encoded_share = STANDARD.encode(&sealed_box);

        let key_pair = get_key_pairs(
            PREVIOUS_PRIVATE_KEY.to_string(),
            CURRENT_PRIVATE_KEY.to_string(),
        );
        let smpc_request = get_mock_request();

        let result = smpc_request.decrypt_iris_share(encoded_share, key_pair);

        assert!(matches!(result, Err(SharesDecodingError::SerdeError(_))));
    }

    #[tokio::test]
    async fn test_validate_iris_share() {
        let mock_iris_codes_json = mock_iris_codes_json();
        let mock_serialized_iris = serde_json::to_string(&mock_iris_codes_json).unwrap();
        let mock_hash = calculate_sha256(mock_serialized_iris.into_bytes());

        let smpc_request = get_mock_smpc_request_with_hashes([
            mock_hash.clone(),
            "dummy_hash_1".to_string(),
            "dummy_hash_2".to_string(),
        ]);

        let is_valid = smpc_request
            .validate_iris_share(0, mock_iris_codes_json)
            .unwrap();

        assert!(is_valid, "The iris share should be valid");
    }

    #[tokio::test]
    async fn test_validate_iris_share_invalid() {
        // Arrange
        let mock_iris_codes_json = mock_iris_codes_json();
        let incorrect_hash = "incorrect_hash_value".to_string();

        let smpc_request = get_mock_smpc_request_with_hashes([
            incorrect_hash,
            "dummy_hash_1".to_string(),
            "dummy_hash_2".to_string(),
        ]);

        // Act
        let is_valid = smpc_request
            .validate_iris_share(0, mock_iris_codes_json)
            .unwrap();

        // Assert
        assert!(!is_valid, "The iris share should be invalid");
    }
}
