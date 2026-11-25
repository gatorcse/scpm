package scpm.testrunner;

import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicBoolean;
import sbt.testing.Framework;

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
    BufferedReader stdin =
        new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
    String line = stdin.readLine();
    if (line == null) badRequest(protocol, null, "no request on stdin");

    Object id = null;
    RunParams params = null;
    try {
      Map<String, Object> request = asObject(Json.parse(line));
      Object requestId = request.get("id");
      // Responses carry an integer id or null, never the request's malformed one.
      if (!(requestId instanceof Long)) throw new IllegalArgumentException("'id' must be an integer");
      id = requestId;
      if (!"run".equals(request.get("method"))) {
        Output.error(protocol, id, "unknown_method", "unknown method: " + request.get("method"));
        System.exit(2);
      }
      params = RunParams.fromJson(asObject(request.get("params")));
    } catch (IllegalArgumentException e) {
      badRequest(protocol, id, e.getMessage());
    } catch (StackOverflowError e) {
      badRequest(protocol, id, "request nested too deeply");
    }

    Output out = new Output(protocol, (Long) id, params.target());
    PrintStream stdout = out.logStream("info");
    PrintStream stderr = out.logStream("error");
    // Before any framework or test class is loaded: their static initializers may print.
    System.setOut(stdout);
    System.setErr(stderr);
    // Tests reading stdin get end of input instead of blocking on the CLI's pipe.
    System.setIn(new ByteArrayInputStream(new byte[0]));
    AtomicBoolean answered = new AtomicBoolean();
    AtomicBoolean parentGone = new AtomicBoolean();
    // Before loading frameworks: one that blocks while starting up must not outlive the CLI either.
    exitWhenParentGoes(stdin, answered, parentGone);
    ClassLoader loader = Main.class.getClassLoader();
    List<Framework> frameworks;
    try {
      frameworks = TestRun.frameworks(params.frameworks(), loader);
    } catch (IllegalArgumentException e) {
      badRequest(protocol, id, e.getMessage());
      return;
    }

    TestRun run = new TestRun(params, frameworks, out, loader);
    // A run the JVM abandons (System.exit in a test, or the CLI went away) is still answered.
    ExitStatus.capture();
    Runtime.getRuntime()
        .addShutdownHook(
            new Thread(
                () -> {
                  if (!answered.compareAndSet(false, true)) return;
                  String reason =
                      parentGone.get()
                          ? PARENT_GONE
                          : ExitStatus.last()
                              .stream()
                              .mapToObj(status -> "a test called System.exit(" + status + ")")
                              .findFirst()
                              .orElse("a test called System.exit");
                  out.result(run.abort(reason));
                }));
    Map<String, Object> result;
    try {
      result = run.run();
    } catch (Throwable t) {
      t.printStackTrace();
      result = run.result("crashed");
    } finally {
      stdout.close();
      stderr.close();
    }
    if (answered.compareAndSet(false, true)) out.result(result);
    // Frameworks such as cats-effect and ZIO can leave non-daemon threads behind.
    System.exit(0);
  }

  private static final String PARENT_GONE = "parent went away (stdin closed)";

  /**
   * The CLI keeps stdin open until the run is answered. End of input before that means it went
   * away: the runner and anything the tests started must not outlive it.
   */
  private static void exitWhenParentGoes(
      BufferedReader stdin, AtomicBoolean answered, AtomicBoolean parentGone) {
    Thread watcher =
        new Thread(
            () -> {
              try {
                while (stdin.read() != -1) {
                  // ignore anything after the request
                }
              } catch (IOException e) {
                // treated like end of input
              }
              if (answered.get()) return;
              parentGone.set(true);
              ProcessHandle.current().descendants().forEach(ProcessHandle::destroyForcibly);
              System.exit(4);
            },
            "scpm-stdin-watcher");
    watcher.setDaemon(true);
    watcher.start();
  }

  private static void badRequest(PrintStream protocol, Object id, String message) {
    Output.error(protocol, id, "bad_request", message);
    System.exit(2);
  }

  @SuppressWarnings("unchecked")
  private static Map<String, Object> asObject(Object json) {
    if (json instanceof Map<?, ?> map) return (Map<String, Object>) map;
    throw new IllegalArgumentException("expected a JSON object");
  }
}
