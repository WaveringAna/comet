#!/bin/sh

if [ "${1:-}" = "--list-models" ]; then
    printf '%s\n' 'provider model context max-out thinking images'
    printf '%s\n' 'openai-codex gpt-5.4 272K 128K yes yes'
    exit 0
fi

error_model=false
steer_model=false
for arg in "$@"; do
    if [ "$arg" = "error-model" ]; then
        error_model=true
    elif [ "$arg" = "steer-model" ]; then
        steer_model=true
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
        *'"type":"steer"'*)
            printf '%s\n' '{"type":"response","command":"steer","success":true}'
            printf '%s\n' '{"type":"message_start","message":{"id":"u2","role":"user","content":"second turn"}}'
            printf '%s\n' '{"type":"message_end","message":{"id":"u2","role":"user","content":"second turn"}}'
            printf '%s\n' '{"type":"message_start","message":{"id":"m2","role":"assistant"}}'
            printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"second turn output"}}'
            printf '%s\n' '{"type":"message_end","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"second turn output"}]}}'
            printf '%s\n' '{"type":"agent_end"}'
            printf '%s\n' '{"type":"agent_settled"}'
            ;;
        *'"type":"prompt"'*)
            cwd=$(pwd)
            printf '%s\n' '{"type":"message_start","message":{"id":"u1","role":"user","content":"hello pi"}}'
            printf '%s\n' '{"type":"message_end","message":{"id":"u1","role":"user","content":"hello pi"}}'
            printf '%s\n' '{"type":"agent_start"}'
            if [ "$error_model" = true ]; then
                printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"the selected pi model is unavailable"}}'
            elif [ "$steer_model" = true ]; then
                printf '%s\n' '{"type":"message_start","message":{"id":"m1","role":"assistant"}}'
                printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"first turn output"}}'
                printf '%s\n' '{"type":"message_end","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"first turn output"}]}}'
            else
                printf '%s\n' "{\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\"}}"
                printf '%s\n' "{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"running in $cwd\"}}"
                printf '%s\n' "{\"type\":\"message_end\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"running in $cwd\"}]}}"
            fi
            if [ "$steer_model" = false ]; then
                printf '%s\n' '{"type":"agent_end"}'
                printf '%s\n' '{"type":"agent_settled"}'
            fi
            ;;
        *'"type":"abort"'*)
            exit 0
            ;;
    esac
done
