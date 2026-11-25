package arith

class StdinSuite extends munit.FunSuite:
  test("stdin is empty, not the protocol pipe") {
    assertEquals(System.in.read(), -1)
  }
