package scpm.testrunner;

import java.io.ByteArrayOutputStream;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;

/** Writes protocol messages as JSON lines to the real stdout. Safe to call from any thread. */
final class Output {
  private final PrintStream out;
  private final long requestId;
  private final String target;

  Output(PrintStream out, long requestId, String target) {
    this.out = out;
    this.requestId = requestId;
    this.target = target;
  }

  /** A new event of type {@code t}, with {@code requestId} and {@code target} filled in. */
  Map<String, Object> newEvent(String t) {
    Map<String, Object> e = new LinkedHashMap<>();
    e.put("t", t);
    e.put("requestId", requestId);
    e.put("target", target);
    return e;
  }

  void event(Map<String, Object> event) {
    Map<String, Object> msg = new LinkedHashMap<>();
    msg.put("method", "event");
    msg.put("params", event);
    write(msg);
  }

  void log(String level, String message) {
    Map<String, Object> e = newEvent("log");
    e.put("level", level);
    e.put("message", message);
    event(e);
  }

  void result(Map<String, Object> result) {
    Map<String, Object> msg = new LinkedHashMap<>();
    msg.put("id", requestId);
    msg.put("result", result);
    write(msg);
  }

  static void error(PrintStream out, Object id, String code, String message) {
    Map<String, Object> error = new LinkedHashMap<>();
    error.put("code", code);
    error.put("message", message);
    Map<String, Object> msg = new LinkedHashMap<>();
    msg.put("id", id);
    msg.put("error", error);
    synchronized (out) {
      out.print(Json.write(msg) + "\n");
      out.flush();
    }
  }

  private void write(Map<String, Object> msg) {
    String line = Json.write(msg) + "\n";
    synchronized (out) {
      out.print(line);
      out.flush();
    }
  }

  /** A stream that turns every line written to it into a {@code log} event. */
  PrintStream logStream(String level) {
    return new PrintStream(new LineLogger(level), true, StandardCharsets.UTF_8);
  }

  private final class LineLogger extends OutputStream {
    private final String level;
    private final ByteArrayOutputStream line = new ByteArrayOutputStream();

    LineLogger(String level) {
      this.level = level;
    }

    @Override
    public synchronized void write(int b) {
      if (b == '\n') emitLine();
      else line.write(b);
    }

    @Override
    public synchronized void write(byte[] bytes, int off, int len) {
      for (int i = off; i < off + len; i++) write(bytes[i]);
    }

    /** Emits a trailing partial line, if any. */
    @Override
    public synchronized void close() {
      if (line.size() > 0) emitLine();
    }

    private void emitLine() {
      String text = line.toString(StandardCharsets.UTF_8);
      line.reset();
      log(level, text.endsWith("\r") ? text.substring(0, text.length() - 1) : text);
    }
  }
}
