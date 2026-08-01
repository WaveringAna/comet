#!/bin/sh

if [ "${1:-}" = "--list-models" ]; then
    printf '%s\n' 'provider model context max-out thinking images'
    printf '%s\n' 'openai-codex gpt-5.4 272K 128K yes yes'
    exit 0
fi

error_model=false
for arg in "$@"; do
    if [ "$arg" = "error-model" ]; then
        error_model=true
    fi
done

while IFS= read -r line; do
    case "$line" in
        *'"id":"nova-model-state"'*'"type":"get_state"'*)
            printf '%s\n' '{"id":"nova-model-state","type":"response","command":"get_state","success":true,"data":{"model":{"id":"gpt-5.4"}}}'
            ;;
        *'"type":"get_state"'*)
            printf '%s\n' '{"id":"nova-state","type":"response","command":"get_state","success":true,"data":{"model":{"id":"gpt-5.4"},"sessionId":"pi-session-1","thinkingLevel":"medium"}}'
            ;;
        *'"type":"prompt"'*)
            cwd=$(pwd)
            printf '%s\n' '{"type":"agent_start"}'
            if [ "$error_model" = true ]; then
                printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"the selected pi model is unavailable"}}'
            else
                printf '%s\n' "{\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\"}}"
                printf '%s\n' "{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"running in $cwd\"}}"
                printf '%s\n' "{\"type\":\"message_end\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"running in $cwd\"}]}}"
            fi
            printf '%s\n' '{"type":"agent_end"}'
            printf '%s\n' '{"type":"agent_settled"}'
            ;;
        *'"type":"abort"'*)
            exit 0
            ;;
    esac
done
