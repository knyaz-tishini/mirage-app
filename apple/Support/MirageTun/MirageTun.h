#pragma once
#include <stddef.h>
#include <stdint.h>

typedef void (*mirage_write_fn)(const uint8_t *data, size_t len, void *ctx);

void *mirage_tun_start_artefact(
    const uint8_t *json,
    size_t json_len,
    mirage_write_fn write_fn,
    void *ctx);

void mirage_tun_process_packet(void *ctx, const uint8_t *data, size_t len);
void mirage_tun_stop(void *ctx);

char *mirage_claim_redeem(const char *code);
void mirage_store_free(char *ptr);
