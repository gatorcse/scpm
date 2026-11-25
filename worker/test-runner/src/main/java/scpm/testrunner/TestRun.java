package scpm.testrunner;

import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Deque;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import sbt.testing.Event;
import sbt.testing.Framework;
import sbt.testing.Logger;
import sbt.testing.NestedTestSelector;
import sbt.testing.Runner;
import sbt.testing.Selector;
import sbt.testing.SuiteSelector;
import sbt.testing.Task;
import sbt.testing.TaskDef;
import sbt.testing.TestSelector;
import sbt.testing.TestWildcardSelector;

/** One test run: discover suites, run them through their frameworks and report every event. */
final class TestRun {
  /** Frameworks detected when the request names none, in priority order. */
  static final List<String> KNOWN_FRAMEWORKS =
      List.of(
          "munit.Framework",
          "org.scalatest.tools.Framework",
          "weaver.framework.CatsEffect",
          "zio.test.sbt.ZTestFramework",
          "utest.runner.Framework",
          "org.specs2.runner.Specs2Framework",
          "com.novocode.junit.JUnitFramework");

  private final RunParams params;
  private final Output out;
  private final ClassLoader loader;
  private int passed;
  private int failed;
  private int skipped;
  private boolean crashed;

  TestRun(RunParams params, Output out, ClassLoader loader) {
    this.params = params;
    this.out = out;
    this.loader = loader;
  }

  Map<String, Object> run() throws ReflectiveOperationException {
    long start = System.nanoTime();
    List<Framework> frameworks = frameworks();
    Discovery discovery = new Discovery(loader);
    List<String> classNames = Discovery.classNames(params.testClassesDirs());
    Set<String> claimed = new HashSet<>();
    for (Framework framework : frameworks) {
      List<TaskDef> taskDefs = new ArrayList<>();
      for (String className : classNames) {
        String testName = Discovery.testName(className);
        if (claimed.contains(className) || !params.selects(testName)) continue;
        discovery
            .match(className, framework.fingerprints())
            .ifPresent(
                fp -> {
                  claimed.add(className);
                  taskDefs.add(new TaskDef(testName, fp, false, selectors()));
                });
      }
      if (!taskDefs.isEmpty()) runFramework(framework, taskDefs);
    }
    Map<String, Object> result = new LinkedHashMap<>();
    result.put("status", crashed ? "crashed" : failed > 0 ? "failed" : "ok");
    result.put("passed", passed);
    result.put("failed", failed);
    result.put("skipped", skipped);
    result.put("durationMs", (System.nanoTime() - start) / 1_000_000);
    return result;
  }

  private List<Framework> frameworks() throws ReflectiveOperationException {
    List<Framework> frameworks = new ArrayList<>();
    if (!params.frameworks().isEmpty()) {
      for (String name : params.frameworks()) frameworks.add(instantiate(name));
      return frameworks;
    }
    for (String name : KNOWN_FRAMEWORKS) {
      try {
        frameworks.add(instantiate(name));
      } catch (ClassNotFoundException | LinkageError e) {
        // not on the test classpath
      }
    }
    return frameworks;
  }

  private Framework instantiate(String name) throws ReflectiveOperationException {
    return (Framework) Class.forName(name, true, loader).getDeclaredConstructor().newInstance();
  }

  private Selector[] selectors() {
    if (params.testNames().isEmpty()) return new Selector[] {new SuiteSelector()};
    return params.testNames().stream().map(TestSelector::new).toArray(Selector[]::new);
  }

  private void runFramework(Framework framework, List<TaskDef> taskDefs) {
    Runner runner = framework.runner(new String[0], new String[0], loader);
    Deque<Task> queue = new ArrayDeque<>(Arrays.asList(runner.tasks(taskDefs.toArray(TaskDef[]::new))));
    Logger[] loggers = {new EventLogger()};
    while (!queue.isEmpty()) {
      Task task = queue.poll();
      String suite = task.taskDef().fullyQualifiedName();
      Map<String, Object> startEvent = out.newEvent("test.suite.start");
      startEvent.put("suite", suite);
      out.event(startEvent);
      long start = System.nanoTime();
      try {
        queue.addAll(Arrays.asList(task.execute(event -> report(event, suite), loggers)));
      } catch (Throwable t) {
        crashed = true;
        out.log("error", "suite " + suite + " crashed:\n" + stackTrace(t));
      }
      Map<String, Object> endEvent = out.newEvent("test.suite.end");
      endEvent.put("suite", suite);
      endEvent.put("durationMs", (System.nanoTime() - start) / 1_000_000);
      out.event(endEvent);
    }
    String summary = runner.done();
    if (summary != null && !summary.isBlank()) out.log("info", summary);
  }

  /**
   * The {@link sbt.testing.EventHandler}: one {@code test.case} event per framework event.
   *
   * <p>JUnit-style frameworks (munit, junit-interface) qualify test names with the suite name;
   * those are attributed to the running suite and the prefix is dropped from the case name.
   */
  private synchronized void report(Event event, String taskSuite) {
    String status =
        switch (event.status()) {
          case Success -> "passed";
          case Failure -> "failed";
          case Error -> "error";
          case Skipped -> "skipped";
          case Ignored -> "ignored";
          case Canceled -> "canceled";
          case Pending -> "pending";
        };
    switch (event.status()) {
      case Success -> passed++;
      case Failure, Error -> failed++;
      default -> skipped++;
    }
    String suite =
        event.fullyQualifiedName().startsWith(taskSuite + ".") ? taskSuite : event.fullyQualifiedName();
    String name = caseName(event.selector(), suite);
    Map<String, Object> e = out.newEvent("test.case");
    e.put("suite", suite);
    e.put("name", name.startsWith(suite + ".") ? name.substring(suite.length() + 1) : name);
    e.put("status", status);
    e.put("durationMs", event.duration() >= 0 ? event.duration() : null);
    boolean failure = status.equals("failed") || status.equals("error");
    if (failure) e.put("failure", failure(event, suite));
    out.event(e);
  }

  private static String caseName(Selector selector, String suite) {
    if (selector instanceof TestSelector s) return s.testName();
    if (selector instanceof NestedTestSelector s) return s.testName();
    if (selector instanceof TestWildcardSelector s) return s.testWildcard();
    return suite;
  }

  private static Map<String, Object> failure(Event event, String suite) {
    Map<String, Object> f = new LinkedHashMap<>();
    Throwable t = event.throwable().isDefined() ? event.throwable().get() : null;
    f.put("message", t == null ? null : String.valueOf(t.getMessage()));
    f.put("stackTrace", t == null ? null : stackTrace(t));
    StackTraceElement frame = t == null ? null : suiteFrame(t, suite);
    f.put("file", frame == null ? null : sourcePath(frame));
    f.put("line", frame == null ? null : frame.getLineNumber());
    return f;
  }

  /** The innermost frame in the suite's own code: where the failing assertion is. */
  private static StackTraceElement suiteFrame(Throwable t, String suite) {
    for (StackTraceElement frame : t.getStackTrace()) {
      String cls = frame.getClassName();
      if ((cls.equals(suite) || cls.startsWith(suite + "$")) && frame.getFileName() != null) {
        return frame;
      }
    }
    return null;
  }

  /**
   * Bytecode only records a source's file name, so this is the conventional path relative to a
   * source root ({@code a/b/FooSuite.scala}); the CLI resolves it against the target's sources.
   */
  private static String sourcePath(StackTraceElement frame) {
    String cls = frame.getClassName();
    int dot = cls.lastIndexOf('.');
    return dot < 0 ? frame.getFileName() : cls.substring(0, dot).replace('.', '/') + "/" + frame.getFileName();
  }

  private static String stackTrace(Throwable t) {
    StringWriter sw = new StringWriter();
    t.printStackTrace(new PrintWriter(sw));
    return sw.toString();
  }

  /** Framework log output becomes {@code log} events. */
  private final class EventLogger implements Logger {
    @Override
    public boolean ansiCodesSupported() {
      return false;
    }

    @Override
    public void error(String msg) {
      out.log("error", msg);
    }

    @Override
    public void warn(String msg) {
      out.log("warn", msg);
    }

    @Override
    public void info(String msg) {
      out.log("info", msg);
    }

    @Override
    public void debug(String msg) {
      out.log("debug", msg);
    }

    @Override
    public void trace(Throwable t) {
      out.log("error", stackTrace(t));
    }
  }
}
