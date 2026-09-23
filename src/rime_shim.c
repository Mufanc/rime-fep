#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <rime_api.h>

typedef struct {
    RimeApi *api;
    char *shared_data_dir;
    char *user_data_dir;
    char *prebuilt_data_dir;
} RimeFep;

static const char *builtin_modules[] = {"default", NULL};

static char *copy_string(const char *string) {
    size_t size = strlen(string) + 1;
    char *copy = malloc(size);
    if (copy) {
        memcpy(copy, string, size);
    }
    return copy;
}

static void append(char *buffer, size_t size, const char *format, ...) {
    size_t used = strlen(buffer);
    if (used >= size - 1) {
        return;
    }

    va_list arguments;
    va_start(arguments, format);
    vsnprintf(buffer + used, size - used, format, arguments);
    va_end(arguments);
}

const char *rime_fep_version(void) {
    RimeApi *api = rime_get_api();
    return api && api->get_version ? api->get_version() : NULL;
}

void *rime_fep_create(const char *shared_data_dir, const char *user_data_dir,
                      const char *prebuilt_data_dir, int deploy,
                      int load_plugins) {
    RimeFep *rime = calloc(1, sizeof(*rime));
    if (!rime) {
        return NULL;
    }

    rime->shared_data_dir = copy_string(shared_data_dir);
    rime->user_data_dir = copy_string(user_data_dir);
    rime->prebuilt_data_dir = copy_string(prebuilt_data_dir);
    if (!rime->shared_data_dir || !rime->user_data_dir ||
        !rime->prebuilt_data_dir) {
        free(rime->shared_data_dir);
        free(rime->user_data_dir);
        free(rime->prebuilt_data_dir);
        free(rime);
        return NULL;
    }

    rime->api = rime_get_api();
    if (!rime->api) {
        free(rime->shared_data_dir);
        free(rime->user_data_dir);
        free(rime->prebuilt_data_dir);
        free(rime);
        return NULL;
    }

    RIME_STRUCT(RimeTraits, traits);
    traits.shared_data_dir = rime->shared_data_dir;
    traits.user_data_dir = rime->user_data_dir;
    traits.distribution_name = "rime-fep";
    traits.distribution_code_name = "rime-fep";
    traits.distribution_version = "0.1.0";
    traits.app_name = "rime.rime-fep";
    traits.min_log_level = 2;
    traits.log_dir = "";
    traits.prebuilt_data_dir = rime->prebuilt_data_dir;
    traits.staging_dir = rime->prebuilt_data_dir;
    if (!load_plugins) {
        traits.modules = builtin_modules;
    }

    rime->api->setup(&traits);
    rime->api->initialize(&traits);
    if (deploy && rime->api->start_maintenance(True)) {
        rime->api->join_maintenance_thread();
    }

    return rime;
}

void rime_fep_destroy(void *pointer) {
    RimeFep *rime = pointer;
    if (!rime) {
        return;
    }
    rime->api->cleanup_all_sessions();
    rime->api->sync_user_data();
    rime->api->finalize();
    free(rime->shared_data_dir);
    free(rime->user_data_dir);
    free(rime->prebuilt_data_dir);
    free(rime);
}

uintptr_t rime_fep_create_session(void *pointer) {
    RimeFep *rime = pointer;
    RimeSessionId session = rime->api->create_session();
    if (session) {
        rime->api->set_option(session, "ascii_mode", False);
    }
    return session;
}

void rime_fep_destroy_session(void *pointer, uintptr_t session) {
    RimeFep *rime = pointer;
    rime->api->destroy_session(session);
}

int rime_fep_process(void *pointer, uintptr_t session, int keycode,
                     int modifiers) {
    RimeFep *rime = pointer;
    return rime->api->process_key(session, keycode, modifiers);
}

int rime_fep_take_commit(void *pointer, uintptr_t session, char *buffer,
                         size_t size) {
    RimeFep *rime = pointer;
    RIME_STRUCT(RimeCommit, commit);
    if (!size || !rime->api->get_commit(session, &commit)) {
        return 0;
    }

    snprintf(buffer, size, "%s", commit.text ? commit.text : "");
    rime->api->free_commit(&commit);
    return 1;
}

int rime_fep_status(void *pointer, uintptr_t session, char *buffer,
                    size_t size) {
    RimeFep *rime = pointer;
    RIME_STRUCT(RimeContext, context);
    if (!size || !rime->api->get_context(session, &context)) {
        return 0;
    }

    buffer[0] = '\0';
    append(buffer, size, "[中]");
    int separated = 0;
    if (context.composition.preedit && context.composition.preedit[0]) {
        append(buffer, size, " %s", context.composition.preedit);
        if (context.menu.num_candidates > 0) {
            append(buffer, size, " |");
            separated = 1;
        }
    }
    for (int index = 0; index < context.menu.num_candidates; ++index) {
        const char *spacing = separated && index == 0 ? " " : "  ";
        const char *label = NULL;
        if (context.select_labels) {
            label = context.select_labels[index];
        }
        if (label && label[0]) {
            append(buffer, size, "%s%s.%s", spacing, label,
                   context.menu.candidates[index].text
                       ? context.menu.candidates[index].text
                       : "");
        } else {
            append(buffer, size, "%s%d.%s", spacing, index + 1,
                   context.menu.candidates[index].text
                       ? context.menu.candidates[index].text
                       : "");
        }
    }

    rime->api->free_context(&context);
    return 1;
}

void rime_fep_clear(void *pointer, uintptr_t session) {
    RimeFep *rime = pointer;
    rime->api->clear_composition(session);
}
