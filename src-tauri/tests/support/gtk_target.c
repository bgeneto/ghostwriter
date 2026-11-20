/*
 * Test target for the Linux end-to-end test.
 *
 * A GTK3 window holding a GtkTextView — a real, multi-line toolkit text widget,
 * which is what the Ghostwriter actually types into (an editor, a browser, a
 * chat box). On Windows the equivalent is an EDIT control; here the toolkit
 * owns the editing and the keysym translation, so this exercises the parts most
 * likely to break: dead keys, compose sequences and Return.
 *
 * It runs as a separate process because GTK insists on owning the main thread,
 * while `cargo test` gives each test a worker thread.
 *
 * Usage: gtk_target <state-file>
 *
 * The state file is rewritten every 50 ms with:
 *   line 1: "ready" once the window is mapped, "waiting" before that
 *   line 2: the window title, so the test can confirm X11 can see it
 *   remainder: the buffer contents, verbatim
 *
 * Writes go through a temp file and a rename, so a reader never observes a
 * half-written payload.
 */

#include <gtk/gtk.h>
#include <gdk/gdkx.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

static const char *state_path = NULL;
static GtkWidget *view = NULL;
static GtkWidget *toplevel = NULL;
static int mapped = 0;

static void write_state(void) {
    if (!state_path) {
        return;
    }
    char tmp[4096];
    snprintf(tmp, sizeof tmp, "%s.tmp", state_path);

    FILE *f = fopen(tmp, "wb");
    if (!f) {
        return;
    }
    fprintf(f, "%s\n", mapped ? "ready" : "waiting");
    fprintf(f, "%s\n", "Ghostwriter E2E target");

    /* Publish the X11 window id of the *toplevel*, not of the text view's
     * internal GdkWindow: input focus set on a child window does not route key
     * events to the widget that owns the keyboard focus. */
    unsigned long xid = 0;
    if (mapped && toplevel && gtk_widget_get_window(toplevel)) {
        xid = GDK_WINDOW_XID(gtk_widget_get_window(toplevel));
    }
    fprintf(f, "%lu\n", xid);

    if (view) {
        GtkTextBuffer *buffer = gtk_text_view_get_buffer(GTK_TEXT_VIEW(view));
        GtkTextIter start, end;
        gtk_text_buffer_get_bounds(buffer, &start, &end);
        char *text = gtk_text_buffer_get_text(buffer, &start, &end, FALSE);
        if (text) {
            fwrite(text, 1, strlen(text), f);
            g_free(text);
        }
    }
    fclose(f);
    rename(tmp, state_path);
}

static gboolean on_tick(gpointer data) {
    (void)data;
    write_state();
    return G_SOURCE_CONTINUE;
}

static void on_map(GtkWidget *widget, gpointer data) {
    (void)widget;
    (void)data;
    mapped = 1;
    write_state();
}

static void on_destroy(void) {
    write_state();
    gtk_main_quit();
}

static void on_signal(int sig) {
    (void)sig;
    gtk_main_quit();
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <state-file>\n", argv[0]);
        return 2;
    }
    state_path = argv[1];

    signal(SIGTERM, on_signal);
    signal(SIGINT, on_signal);

    gtk_init(&argc, &argv);

    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    toplevel = window;
    gtk_window_set_title(GTK_WINDOW(window), "Ghostwriter E2E target");
    gtk_window_set_default_size(GTK_WINDOW(window), 700, 260);
    /* Near the top-left so focus is unambiguous. */
    gtk_window_move(GTK_WINDOW(window), 40, 40);

    /* A multi-line view: Return must insert a newline, not commit. */
    view = gtk_text_view_new();
    gtk_text_view_set_wrap_mode(GTK_TEXT_VIEW(view), GTK_WRAP_WORD_CHAR);

    GtkWidget *scroller = gtk_scrolled_window_new(NULL, NULL);
    gtk_container_add(GTK_CONTAINER(scroller), view);
    gtk_container_add(GTK_CONTAINER(window), scroller);

    g_signal_connect(window, "destroy", G_CALLBACK(on_destroy), NULL);
    g_signal_connect(window, "map", G_CALLBACK(on_map), NULL);

    gtk_widget_show_all(window);
    gtk_window_present(GTK_WINDOW(window));
    gtk_widget_grab_focus(view);

    g_timeout_add(50, on_tick, NULL);
    write_state();

    gtk_main();
    return 0;
}
