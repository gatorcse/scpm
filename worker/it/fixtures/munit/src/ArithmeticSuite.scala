package arith

class ArithmeticSuite extends munit.FunSuite:
  test("addition works") {
    println("hello from a test")
    assertEquals(1 + 1, 2)
  }

  test("subtraction is broken") {
    assertEquals(3 - 1, 1)
  }
