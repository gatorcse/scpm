package calc;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

public class CalcTest {
  @Test
  public void adds() {
    assertEquals(4, 2 + 2);
  }

  @Test
  public void divides() {
    assertEquals(4, 7 / 2);
  }
}
