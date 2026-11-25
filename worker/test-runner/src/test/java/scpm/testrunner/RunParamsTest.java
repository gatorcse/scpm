package scpm.testrunner;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertThrows;
import static org.junit.Assert.assertTrue;

import java.util.List;
import java.util.Map;
import org.junit.Test;

public class RunParamsTest {
  @Test
  public void globsMatchWholeNames() {
    assertTrue(RunParams.glob("a.b.*Suite").matcher("a.b.FooSuite").matches());
    assertFalse(RunParams.glob("a.b.*Suite").matcher("a.bc.FooSuite").matches());
    assertTrue(RunParams.glob("*Spec?").matcher("x.MySpec2").matches());
    assertFalse(RunParams.glob("Foo").matcher("a.Foo").matches());
  }

  @Test
  public void optionalListsDefaultToEmpty() {
    RunParams params = RunParams.fromJson(Map.of("target", "t", "testClassesDirs", List.of("/c")));
    assertEquals(List.of("/c"), params.testClassesDirs());
    assertEquals(List.of(), params.frameworks());
    assertTrue(params.selects("anything.AtAll"));
  }

  @Test
  public void filtersSelectClasses() {
    RunParams params =
        RunParams.fromJson(
            Map.of("target", "t", "testClassesDirs", List.of(), "filters", List.of("*.FastSuite")));
    assertTrue(params.selects("a.FastSuite"));
    assertFalse(params.selects("a.SlowSuite"));
  }

  @Test
  public void onlyAFilterWithoutWildcardsNamesASuite() {
    RunParams params =
        RunParams.fromJson(
            Map.of(
                "target", "t", "testClassesDirs", List.of(), "filters", List.of("a.Exact", "*.Glob")));
    assertTrue(params.names("a.Exact"));
    assertFalse(params.names("a.Glob"));
    assertFalse(params.names("b.Exact"));
  }

  @Test
  public void rejectsMissingFields() {
    assertThrows(IllegalArgumentException.class, () -> RunParams.fromJson(Map.of("target", "t")));
  }
}
