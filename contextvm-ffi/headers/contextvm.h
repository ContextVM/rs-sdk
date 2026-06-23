/*
 * contextvm.h — C FFI header for the ContextVM SDK
 *
 * Usage:
 *   #include "contextvm.h"
 *   Link against libcontextvm_ffi.a (static) or libcontextvm_ffi.so (shared)
 *
 * ═══════════════════════════════════════════════════════════════════════════════
 * ERROR HANDLING CONTRACT
 * ═══════════════════════════════════════════════════════════════════════════════
 *
 * All functions take a CvmError **error out-parameter.
 *   - On success: *error is left untouched (caller should initialize to NULL)
 *   - On failure: *error is set to a freshly-allocated CvmError object
 *
 * IMPORTANT: Error objects are heap-allocated. Callers MUST free them with
 * cvm_error_free() to avoid memory leaks. This applies even if you ignore the
 * error details - always call cvm_error_free(*error) after checking the function
 * return value.
 *
 * Example:
 *   CvmError *err = NULL;
 *   bool ok = cvm_server_ch_recv(handle, &req, &err);
 *   if (!ok) {
 *     fprintf(stderr, "Error: %s\n", cvm_error_message(err));
 *     cvm_error_free(err);  // REQUIRED - frees the error object
 *   }
 *
 * ═══════════════════════════════════════════════════════════════════════════════
 * RECEIVE CALLS AND TIMEOUTS
 * ═══════════════════════════════════════════════════════════════════════════════
 *
 * Blocking receive functions (cvm_*_ch_recv) block indefinitely until a message
 * arrives. These are suitable for dedicated worker threads.
 *
 * Timed receive functions (cvm_*_ch_recv_timeout) accept a timeout_secs parameter
 * and return CVM_TIMEOUT if no message arrives within that duration. These are
 * preferred for embedding in Python/Swift/Kotlin runtimes where you may not want
 * to manage dedicated threads.
 *
 * Passing timeout_secs = 0 is equivalent to a try-recv: it returns immediately,
 * yielding CVM_TIMEOUT if no message is already buffered. Use this for non-blocking
 * polls instead of blocking worker-thread consumption.
 *
 * ═══════════════════════════════════════════════════════════════════════════════
 * MEMORY MANAGEMENT
 * ═══════════════════════════════════════════════════════════════════════════════
 *
 * All functions that return handles use the pattern:
 *   - On success: returns a valid handle (id > 0)
 *   - On error: returns handle { id: 0 } and sets *error
 *
 * Strings returned by this library must be freed with cvm_string_free().
 * Structs with owned strings must be freed with their respective _free functions.
 * Error pointers must be freed with cvm_error_free() - see ERROR HANDLING above.
 */

#ifndef CONTEXTVM_FFI_H
#define CONTEXTVM_FFI_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ─── Opaque Handle ────────────────────────────────────────────────── */

typedef struct {
    uint64_t id;
} CvmHandle;

/* ─── Error ────────────────────────────────────────────────────────── */

typedef enum {
    CVM_OK = 0,
    CVM_TRANSPORT = 1,
    CVM_ENCRYPTION = 2,
    CVM_DECRYPTION = 3,
    CVM_TIMEOUT = 4,
    CVM_VALIDATION = 5,
    CVM_UNAUTHORIZED = 6,
    CVM_SERIALIZATION = 7,
    CVM_OTHER = 99,
} CvmErrorCode;

typedef struct CvmError CvmError;

/* ─── Enums ─────────────────────────────────────────────────────────── */

typedef enum {
    CVM_ENCRYPTION_OPTIONAL = 0,
    CVM_ENCRYPTION_REQUIRED = 1,
    CVM_ENCRYPTION_DISABLED = 2,
} CvmEncryptionMode;

typedef enum {
    CVM_GIFTWRAP_OPTIONAL = 0,
    CVM_GIFTWRAP_EPHEMERAL = 1,
    CVM_GIFTWRAP_PERSISTENT = 2,
} CvmGiftWrapMode;

typedef enum {
    CVM_MSG_REQUEST = 0,
    CVM_MSG_RESPONSE = 1,
    CVM_MSG_ERROR_RESPONSE = 2,
    CVM_MSG_NOTIFICATION = 3,
} CvmJsonRpcType;

/* ─── Structs ──────────────────────────────────────────────────────── */

typedef struct {
    int32_t msg_type;   /* one of CvmJsonRpcType */
    char *payload_json; /* owned JSON string */
    char *method;       /* owned, may be NULL */
    char *id;           /* owned, may be NULL */
} CvmJsonRpcMessage;

typedef struct {
    CvmJsonRpcMessage message;
    char *client_pubkey; /* owned hex string */
    char *event_id;      /* owned hex string */
    bool is_encrypted;
} CvmIncomingRequest;

typedef struct {
    char *pubkey;   /* owned hex string */
    char *name;     /* owned, may be NULL */
    char *version;  /* owned, may be NULL */
    char *picture;  /* owned, may be NULL */
    char *about;    /* owned, may be NULL */
    char *website;  /* owned, may be NULL */
    char *event_id; /* owned hex string */
} CvmServerAnnouncement;

typedef struct {
    char *provider_pubkey;       /* owned hex string */
    char *provider_display_name; /* owned, may be NULL */
    char *provider_name;         /* owned, may be NULL */
    char *provider_about;        /* owned, may be NULL */
    char *provider_picture;      /* owned, may be NULL */
    char *provider_nip05;        /* owned, may be NULL */
    char *tool_name;             /* owned */
    char *description;           /* owned */
    char *schema_json;           /* owned JSON string */
} CvmDiscoveredTool;

typedef struct {
    char *pubkey;  /* owned hex string */
    char *name;    /* owned, may be NULL */
    char *about;   /* owned, may be NULL */
    char *picture; /* owned, may be NULL */
    char *nip05;   /* owned, may be NULL */
} CvmProviderProfile;

typedef struct {
    char *method; /* required */
    char *name;   /* optional */
} CvmCapabilityExclusion;

typedef struct {
    bool supports_encryption;
    bool supports_ephemeral_encryption;
    bool supports_oversized_transfer;
} CvmPeerCapabilities;

typedef struct {
    char **relay_urls;
    size_t relay_url_count;
    int32_t encryption_mode; /* one of CvmEncryptionMode */
    int32_t gift_wrap_mode;  /* one of CvmGiftWrapMode */
    bool is_announced_server;
    char *server_name;    /* may be NULL */
    char *server_version; /* may be NULL */
    char *server_picture; /* may be NULL */
    char *server_about;   /* may be NULL */
    char *server_website; /* may be NULL */
    char **allowed_pubkeys; /* if count > 0, pointer and entries must be non-NULL UTF-8 */
    size_t allowed_pubkey_count;
    uint64_t session_timeout_secs;
    uint64_t cleanup_interval_secs;
    CvmCapabilityExclusion *excluded_capabilities;
    size_t excluded_capability_count;
    size_t max_sessions; /* 0 keeps SDK default */
    uint64_t request_timeout_secs; /* 0 keeps SDK default */
    char **relay_list_urls;
    size_t relay_list_url_count;
    char **bootstrap_relay_urls;
    size_t bootstrap_relay_url_count;
    bool publish_relay_list;
    char *profile_metadata_json; /* optional ProfileMetadata JSON object */
} CvmServerConfig;

typedef struct {
    char **relay_urls;
    size_t relay_url_count;
    char *server_pubkey; /* required */
    int32_t encryption_mode; /* one of CvmEncryptionMode */
    int32_t gift_wrap_mode;  /* one of CvmGiftWrapMode */
    bool is_stateless;
    uint64_t timeout_secs;
    char **discovery_relay_urls;
    size_t discovery_relay_url_count;
    char **fallback_operational_relay_urls;
    size_t fallback_operational_relay_url_count;
} CvmClientConfig;

/* ─── Free Functions ───────────────────────────────────────────────── */

void cvm_string_free(char *s);
void cvm_message_free(CvmJsonRpcMessage msg);
void cvm_incoming_request_free(CvmIncomingRequest req);
void cvm_announcements_free(CvmServerAnnouncement *announcements, size_t count);
void cvm_discovered_tools_free(CvmDiscoveredTool *tools, size_t count);
void cvm_provider_profiles_free(CvmProviderProfile *profiles, size_t count);
void cvm_error_free(CvmError *e);
char *cvm_error_message(const CvmError *e);
CvmErrorCode cvm_error_code(const CvmError *e);

/* ─── Version ──────────────────────────────────────────────────────── */

char *cvm_version(void);

/* ─── Keys / Signer ────────────────────────────────────────────────── */

CvmHandle cvm_keys_generate(CvmError **error);
CvmHandle cvm_keys_from_secret_key(const char *sk, CvmError **error);
char *cvm_keys_public_key(CvmHandle handle, CvmError **error);
char *cvm_keys_secret_key(CvmHandle handle, CvmError **error);
void cvm_keys_free(CvmHandle handle);

/* ─── Relay Pool ───────────────────────────────────────────────────── */

CvmHandle cvm_relay_pool_new(CvmHandle keys_handle, CvmError **error);
bool cvm_relay_pool_connect(CvmHandle pool_handle, char **urls, size_t count, CvmError **error);
bool cvm_relay_pool_disconnect(CvmHandle pool_handle, CvmError **error);
void cvm_relay_pool_free(CvmHandle handle);

/* ─── Server (channel-based) ──────────────────────────────────────── */

CvmHandle cvm_server_ch_new(CvmHandle keys_handle, CvmServerConfig config, CvmError **error);
bool cvm_server_ch_recv(CvmHandle handle, CvmIncomingRequest *out_req, CvmError **error);
bool cvm_server_ch_recv_timeout(CvmHandle handle, uint64_t timeout_secs, CvmIncomingRequest *out_req, CvmError **error);
bool cvm_server_ch_send_response(CvmHandle handle, const char *event_id, const char *payload_json, CvmError **error);
bool cvm_server_ch_send_notification(CvmHandle handle, const char *client_pubkey, const char *payload_json, const char *correlated_event_id, CvmError **error);
bool cvm_server_ch_broadcast_notification(CvmHandle handle, const char *payload_json, CvmError **error);
bool cvm_server_ch_set_announcement_extra_tags(CvmHandle handle, const char *tags_json, CvmError **error);
bool cvm_server_ch_set_announcement_pricing_tags(CvmHandle handle, const char *tags_json, CvmError **error);
bool cvm_server_ch_announce(CvmHandle handle, CvmError **error);
char *cvm_server_ch_announce_event_id(CvmHandle handle, CvmError **error);
char *cvm_server_ch_publish_tools(CvmHandle handle, const char *tools_json, CvmError **error);
char *cvm_server_ch_publish_resources(CvmHandle handle, const char *resources_json, CvmError **error);
char *cvm_server_ch_publish_prompts(CvmHandle handle, const char *prompts_json, CvmError **error);
char *cvm_server_ch_publish_resource_templates(CvmHandle handle, const char *templates_json, CvmError **error);
bool cvm_server_ch_delete_announcements(CvmHandle handle, const char *reason, CvmError **error);
bool cvm_server_ch_close(CvmHandle handle, CvmError **error);

/* ─── Client (channel-based) ──────────────────────────────────────── */

CvmHandle cvm_client_ch_new(CvmHandle keys_handle, CvmClientConfig config, CvmError **error);
bool cvm_client_ch_send(CvmHandle handle, const char *payload_json, CvmError **error);
bool cvm_client_ch_recv(CvmHandle handle, CvmJsonRpcMessage *out_msg, CvmError **error);
bool cvm_client_ch_recv_timeout(CvmHandle handle, uint64_t timeout_secs, CvmJsonRpcMessage *out_msg, CvmError **error);
bool cvm_client_ch_discovered_server_capabilities(CvmHandle handle, CvmPeerCapabilities *out_caps, CvmError **error);
bool cvm_client_ch_server_supports_ephemeral_encryption(CvmHandle handle, CvmError **error);
char *cvm_client_ch_server_initialize_event_json(CvmHandle handle, CvmError **error);
bool cvm_client_ch_close(CvmHandle handle, CvmError **error);

/* ─── Gateway (channel-based) ─────────────────────────────────────── */

CvmHandle cvm_gateway_ch_new(CvmHandle keys_handle, CvmServerConfig config, CvmError **error);
bool cvm_gateway_ch_recv(CvmHandle handle, CvmIncomingRequest *out_req, CvmError **error);
bool cvm_gateway_ch_recv_timeout(CvmHandle handle, uint64_t timeout_secs, CvmIncomingRequest *out_req, CvmError **error);
bool cvm_gateway_ch_send_response(CvmHandle handle, const char *event_id, const char *payload_json, CvmError **error);
bool cvm_gateway_ch_announce(CvmHandle handle, CvmError **error);
char *cvm_gateway_ch_announce_event_id(CvmHandle handle, CvmError **error);
bool cvm_gateway_ch_is_active(CvmHandle handle, CvmError **error);
bool cvm_gateway_ch_stop(CvmHandle handle, CvmError **error);

/* ─── Proxy (channel-based) ───────────────────────────────────────── */

CvmHandle cvm_proxy_ch_new(CvmHandle keys_handle, CvmClientConfig config, CvmError **error);
bool cvm_proxy_ch_send(CvmHandle handle, const char *payload_json, CvmError **error);
bool cvm_proxy_ch_recv(CvmHandle handle, CvmJsonRpcMessage *out_msg, CvmError **error);
bool cvm_proxy_ch_recv_timeout(CvmHandle handle, uint64_t timeout_secs, CvmJsonRpcMessage *out_msg, CvmError **error);
bool cvm_proxy_ch_is_active(CvmHandle handle, CvmError **error);
bool cvm_proxy_ch_stop(CvmHandle handle, CvmError **error);

/* ─── Discovery ────────────────────────────────────────────────────── */

CvmServerAnnouncement *cvm_discover_servers(
    CvmHandle pool_handle,
    char **relay_urls,
    size_t url_count,
    size_t *out_count,
    CvmError **error
);
CvmDiscoveredTool *cvm_discover_tools(
    CvmHandle pool_handle,
    const char *provider_pubkey_hex,
    const char *provider_display_name,
    char **relay_urls,
    size_t url_count,
    size_t *out_count,
    CvmError **error
);
CvmDiscoveredTool *cvm_discover_all_tools(
    CvmHandle pool_handle,
    char **relay_urls,
    size_t url_count,
    size_t *out_count,
    CvmError **error
);
CvmProviderProfile *cvm_fetch_provider_profiles(
    CvmHandle pool_handle,
    char **provider_pubkeys,
    size_t provider_pubkey_count,
    char **relay_urls,
    size_t url_count,
    size_t *out_count,
    CvmError **error
);

/* ─── Encryption ───────────────────────────────────────────────────── */

char *cvm_encrypt_nip44(CvmHandle keys_handle, const char *recipient_hex, const char *plaintext, CvmError **error);
char *cvm_decrypt_nip44(CvmHandle keys_handle, const char *sender_hex, const char *ciphertext, CvmError **error);
char *cvm_pubkey_hex_to_npub(const char *pubkey_hex, CvmError **error);

#ifdef __cplusplus
}
#endif

#endif /* CONTEXTVM_FFI_H */
