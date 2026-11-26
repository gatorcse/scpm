package scpm.testrunner;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertThrows;
import static org.junit.Assert.assertTrue;

import java.util.List;
import org.junit.Test;

public class TestRunTest {
  private final ClassLoader loader = TestRunTest.class.getClassLoader();

  @Test
  public void detectsTheCommonFrameworks() {
    assertTrue(TestRun.KNOWN_FRAMEWORKS.contains("org.scalacheck.ScalaCheckFramework"));
    assertTrue(TestRun.KNOWN_FRAMEWORKS.contains("munit.Framework"));
  }

  @Test
  public void autoDetectionSkipsFrameworksThatAreNotOnTheClasspath() {
    // junit-interface is on this test classpath; the Scala frameworks are not.
    assertEquals(
        List.of("com.novocode.junit.JUnitFramework"),
        TestRun.frameworks(List.of(), loader).stream().map(f -> f.getClass().getName()).toList());
  }

  /** A framework class that is on the classpath but cannot be created. */
  public static final class BrokenFramework {
    public BrokenFramework() {
      throw new IllegalStateException("missing dependency");
    }
  }

  @Test
  public void autoDetectionReportsAFrameworkThatIsPresentButCannotBeCreated() {
    String broken = BrokenFramework.class.getName();
    IllegalArgumentException e =
        assertThrows(
            IllegalArgumentException.class,
            () -> TestRun.detect(List.of("no.Such", broken, "com.novocode.junit.JUnitFramework"), loader));
    assertEquals(
        "cannot create framework " + broken + ": java.lang.IllegalStateException: missing dependency",
        e.getMessage());
  }

  @Test
  public void aNamedFrameworkMustBeOnTheClasspath() {
    IllegalArgumentException e =
        assertThrows(
            IllegalArgumentException.class, () -> TestRun.frameworks(List.of("no.Such"), loader));
    assertEquals("framework no.Such is not on the classpath", e.getMessage());
  }
}
