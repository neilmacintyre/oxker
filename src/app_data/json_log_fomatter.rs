

pub mod log_sanitizer {
    use serde_json::Value;
    // {"message":"LOG MESSAGE CONTENTS","level":"WARNING","timestamp_iso":"2024-04-19T13:36:09.089736-04:00"}

    // Color based on level, and prefix with stipped down time stamp

    pub fn json_formatter(log_json: &str)->String{
        // Parse JSON into a serde_json::Value
        let parsed_json: Value = serde_json::from_str(&log_json).expect("Failed to parse JSON");
        let mut formated_string:  String = "".to_string();

        // Extract the "message" field from the parsed JSON
        if let Some(message) = parsed_json.get("message") {
            // Check if the "message" field is a string
            if let Some(message_str) = message.as_str() {
                formated_string=message_str.to_string();
            } else {
                println!("Message field is not a string");
            }
        } else {
            formated_string="Invalid Format:: <PUT log_json>".to_string();
        }

        formated_string
    }


    #[test]
        /// Get mut container by id
    fn test_json_formatter() {
        let formatted_log = json_formatter("{\"message\":\"LOG MESSAGE CONTENTS\",\"level\":\"WARNING\",\"timestamp_iso\":\"2024-04-19T13:36:09.089736-04:00\"}");
        println!("{formatted_log}");
        assert_eq!(formatted_log, "LOG MESSAGE CONTENTS");
    }


}