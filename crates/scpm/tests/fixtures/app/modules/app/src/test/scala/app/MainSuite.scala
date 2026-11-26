package app

class MainSuite extends munit.FunSuite:
  test("greets") {
    assertEquals(util.Greeting.hello(List("a", "b")), "Hello, a; Hello, b!")
  }
  test("knows its platform") {
    assertEquals(util.Greeting.version, "scala 3")
  }
