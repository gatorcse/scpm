package scpm.testrunner;

import java.io.BufferedReader;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;

/**
 * The scpm test runner: reads one {@code run} request from stdin, runs the tests, writes the
 * response and exits. Everything the tests print becomes {@code log} events, so stdout carries
 * protocol messages only.
 */
public final class Main {
  private Main() {}

  public static void main(String[] args) throws IOException {
    PrintStream protocol =
        new PrintStream(new FileOutputStream(FileDescriptor.out), false, StandardCharsets.UTF_8);
    String line =
        new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8)).readLine();
    if (line == null) {
      Output.error(protocol, null, "bad_request", "no request on stdin");
      System.exit(2);
    }

    Object id = null;
    RunParams params;
    try {
      Map<String, Object> request = asObject(Json.parse(line));
      id = request.get("id");
      if (!(id instanceof Long)) throw new IllegalArgumentException("'id' must be an integer");
      if (!"run".equals(request.get("method"))) {
        Output.error(protocol, id, "unknown_method", "unknown method: " + request.get("method"));
        System.exit(2);
      }
      params = RunParams.fromJson(asObject(request.get("params")));
    } catch (IllegalArgumentException e) {
      Output.error(protocol, id, "bad_request", e.getMessage());
      System.exit(2);
      return;
    }

    Output out = new Output(protocol, (Long) id, params.target());
    PrintStream stdout = out.logStream("info");
    PrintStream stderr = out.logStream("error");
    System.setOut(stdout);
    System.setErr(stderr);
    Map<String, Object> result;
    try {
      result = new TestRun(params, out, Main.class.getClassLoader()).run();
    } catch (Throwable t) {
      t.printStackTrace();
      result = Map.of("status", "crashed", "passed", 0, "failed", 0, "skipped", 0, "durationMs", 0);
    } finally {
      stdout.close();
      stderr.close();
    }
    out.result(result);
    // Frameworks such as cats-effect and ZIO can leave non-daemon threads behind.
    System.exit(0);
  }

  @SuppressWarnings("unchecked")
  private static Map<String, Object> asObject(Object json) {
    if (json instanceof Map<?, ?> map) return (Map<String, Object>) map;
    throw new IllegalArgumentException("expected a JSON object");
  }
}
