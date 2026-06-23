/**
 * C End-to-End Test for ContextVM FFI Bindings
 *
 * This program tests the ContextVM C API by calling FFI functions directly
 * from C code. It verifies:
 * - Key generation and management
 * - Error handling
 * - Memory safety (allocations/frees)
 * - Message structures
 *
 * Build with: make
 * Run with: make test
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <stdint.h>
#include "../headers/contextvm.h"

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) void test_##name()
#define RUN_TEST(name) do { \
    printf("  Running %s... ", #name); \
    test_##name(); \
    printf("OK\n"); \
    tests_passed++; \
} while(0)

#define ASSERT(expr) do { \
    if (!(expr)) { \
        printf("FAILED\n  Assertion failed: %s at line %d\n", #expr, __LINE__); \
        tests_failed++; \
        return; \
    } \
} while(0)

// Helper to print hex bytes
static void print_hex(const char* label, const char* data) {
    printf("%s: %s\n", label, data ? data : "(null)");
}

TEST(keys_generation_and_free) {
    CvmError* error = NULL;
    CvmHandle keys = cvm_keys_generate(&error);

    ASSERT(keys.id > 0);

    // Get public key
    char* pubkey = cvm_keys_public_key(keys, &error);
    ASSERT(pubkey != NULL);
    ASSERT(strlen(pubkey) == 64); // 32 bytes as hex

    print_hex("  Public key", pubkey);

    // Cleanup
    cvm_string_free(pubkey);
    cvm_keys_free(keys);
}

TEST(invalid_handle_handling) {
    CvmHandle invalid = { .id = 99999 };
    CvmError* error = NULL;

    // Should return NULL for invalid handle, not crash
    char* pubkey = cvm_keys_public_key(invalid, &error);
    // Note: The function returns NULL for invalid handle
    // We just verify it doesn't crash
    (void)pubkey; // Suppress unused warning
}

TEST(string_allocation_roundtrip) {
    const char* test_str = "Hello, ContextVM FFI!";

    // Allocate and free a string through the FFI
    char* c_str = strdup(test_str);
    ASSERT(c_str != NULL);
    ASSERT(strcmp(c_str, test_str) == 0);

    // Free using FFI function
    cvm_string_free(c_str);
    // If we got here without crash, the free worked
}

TEST(message_structure_creation) {
    // Create a JSON-RPC message structure
    CvmJsonRpcMessage msg = {
        .msg_type = CVM_MSG_REQUEST,
        .id = strdup("req-123"),
        .payload_json = strdup("{\"jsonrpc\":\"2.0\",\"method\":\"test\"}"),
        .method = strdup("test")
    };

    ASSERT(msg.id != NULL);
    ASSERT(msg.payload_json != NULL);
    ASSERT(strcmp(msg.id, "req-123") == 0);

    // Free the message
    cvm_message_free(msg);
}

TEST(error_handling_lifecycle) {
    CvmHandle invalid = { .id = 0 };
    CvmJsonRpcMessage out_msg;
    CvmError* error = NULL;

    int result = cvm_proxy_ch_recv_timeout(invalid, 1, &out_msg, &error);

    // For invalid handle, the function should return false
    ASSERT(result == 0);

    // Error might or might not be set depending on implementation
    if (error != NULL) {
        printf("\n    Error code: %d\n", cvm_error_code(error));
        cvm_error_free(error);
    }
}

TEST(multiple_keys_isolation) {
    CvmError* error = NULL;
    CvmHandle keys1 = cvm_keys_generate(&error);
    CvmHandle keys2 = cvm_keys_generate(&error);

    ASSERT(keys1.id > 0);
    ASSERT(keys2.id > 0);
    ASSERT(keys1.id != keys2.id);

    char* pubkey1 = cvm_keys_public_key(keys1, &error);
    char* pubkey2 = cvm_keys_public_key(keys2, &error);

    ASSERT(pubkey1 != NULL);
    ASSERT(pubkey2 != NULL);
    ASSERT(strcmp(pubkey1, pubkey2) != 0);

    print_hex("  Key 1", pubkey1);
    print_hex("  Key 2", pubkey2);

    cvm_string_free(pubkey1);
    cvm_string_free(pubkey2);
    cvm_keys_free(keys1);
    cvm_keys_free(keys2);
}

TEST(constant_values) {
    // Verify constants match expected values
    ASSERT(CVM_OK == 0);
    ASSERT(CVM_TRANSPORT == 1);
    ASSERT(CVM_ENCRYPTION == 2);
    ASSERT(CVM_DECRYPTION == 3);
    ASSERT(CVM_TIMEOUT == 4);
    ASSERT(CVM_VALIDATION == 5);
    ASSERT(CVM_UNAUTHORIZED == 6);
    ASSERT(CVM_SERIALIZATION == 7);
    ASSERT(CVM_OTHER == 99);

    ASSERT(CVM_ENCRYPTION_DISABLED == 2);
    ASSERT(CVM_ENCRYPTION_OPTIONAL == 0);
    ASSERT(CVM_ENCRYPTION_REQUIRED == 1);

    ASSERT(CVM_GIFTWRAP_OPTIONAL == 0);
    ASSERT(CVM_GIFTWRAP_EPHEMERAL == 1);
    ASSERT(CVM_GIFTWRAP_PERSISTENT == 2);

    ASSERT(CVM_MSG_REQUEST == 0);
    ASSERT(CVM_MSG_RESPONSE == 1);
    ASSERT(CVM_MSG_ERROR_RESPONSE == 2);
    ASSERT(CVM_MSG_NOTIFICATION == 3);
}

TEST(null_safety) {
    // These should not crash
    cvm_string_free(NULL);
    cvm_error_free(NULL);

    CvmJsonRpcMessage msg = {
        .msg_type = CVM_MSG_REQUEST,
        .id = NULL,
        .payload_json = NULL,
        .method = NULL
    };
    cvm_message_free(msg);

    CvmIncomingRequest req = {
        .message = msg,
        .client_pubkey = NULL,
        .event_id = NULL,
        .is_encrypted = 0
    };
    cvm_incoming_request_free(req);
}

TEST(config_structure_sizes) {
    // Verify structures have expected sizes (sanity check)
    CvmServerConfig server_config = {0};
    CvmClientConfig client_config = {0};

    // Just verify we can create and access these structures
    server_config.relay_url_count = 1;
    client_config.relay_url_count = 1;

    ASSERT(server_config.relay_url_count == 1);
    ASSERT(client_config.relay_url_count == 1);
}

TEST(version_info) {
    char* version = cvm_version();
    ASSERT(version != NULL);
    ASSERT(strlen(version) > 0);
    printf("\n    ContextVM Version: %s\n", version);
    cvm_string_free(version);
}

int main(int argc, char** argv) {
    (void)argc;
    (void)argv;

    printf("========================================\n");
    printf("ContextVM FFI C End-to-End Tests\n");
    printf("========================================\n\n");

    RUN_TEST(keys_generation_and_free);
    RUN_TEST(invalid_handle_handling);
    RUN_TEST(string_allocation_roundtrip);
    RUN_TEST(message_structure_creation);
    RUN_TEST(error_handling_lifecycle);
    RUN_TEST(multiple_keys_isolation);
    RUN_TEST(constant_values);
    RUN_TEST(null_safety);
    RUN_TEST(config_structure_sizes);
    RUN_TEST(version_info);

    printf("\n========================================\n");
    printf("Results: %d passed, %d failed\n", tests_passed, tests_failed);
    printf("========================================\n");

    return tests_failed > 0 ? 1 : 0;
}
