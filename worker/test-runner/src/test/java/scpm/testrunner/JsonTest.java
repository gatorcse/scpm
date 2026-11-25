package scpm.testrunner;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertThrows;

import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.junit.Test;

public class JsonTest {
  @Test
  public void parsesNestedValues() {
    Object value = Json.parse(" {\"id\": 1, \"params\": {\"xs\": [\"a\", 2.5, true, null], \"s\": \"q\\\"\\n\\u00e9\"}} ");
    Map<String, Object> params = new LinkedHashMap<>();
    params.put("xs", Arrays.asList("a", 2.5, true, null));
    params.put("s", "q\"\né");
    Map<String, Object> expected = new LinkedHashMap<>();
    expected.put("id", 1L);
    expected.put("params", params);
    assertEquals(expected, value);
  }

  @Test
  public void writesEscapedStrings() {
    Map<String, Object> value = new LinkedHashMap<>();
    value.put("message", "tab\there \"quoted\" \u0001");
    value.put("items", List.of(1, false));
    value.put("none", null);
    assertEquals(
        "{\"message\":\"tab\\there \\\"quoted\\\" \\u0001\",\"items\":[1,false],\"none\":null}",
        Json.write(value));
  }

  @Test
  public void roundTrips() {
    String json = "{\"a\":[1,{\"b\":\"\\\\\"}],\"c\":-3.5e2}";
    assertEquals(Json.parse(json), Json.parse(Json.write(Json.parse(json))));
  }

  @Test
  public void rejectsMalformedInput() {
    assertThrows(IllegalArgumentException.class, () -> Json.parse("{\"a\": }"));
    assertThrows(IllegalArgumentException.class, () -> Json.parse("[1, 2"));
    assertThrows(IllegalArgumentException.class, () -> Json.parse("{} extra"));
  }

  @Test
  public void capsNestingDepth() {
    String ok = "[".repeat(Json.MAX_DEPTH) + "]".repeat(Json.MAX_DEPTH);
    assertEquals(Json.parse(ok), Json.parse(ok));
    String deep = "[".repeat(100_000) + "]".repeat(100_000);
    assertThrows(IllegalArgumentException.class, () -> Json.parse(deep));
  }
}
