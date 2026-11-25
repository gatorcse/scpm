package scpm.worker

import java.io.File
import java.util.Optional

class DiagnosticsSuite extends munit.FunSuite:
  private def some(i: Int): Optional[Integer] = Optional.of(i)

  private def position(
      caretLine: Optional[Integer] = Optional.empty,
      caretColumn: Optional[Integer] = Optional.empty,
      start: Option[(Int, Int)] = None,
      end: Option[(Int, Int)] = None,
      content: String = ""
  ): xsbti.Position = new:
    def line(): Optional[Integer] = caretLine
    def lineContent(): String = content
    def offset(): Optional[Integer] = Optional.empty
    def pointer(): Optional[Integer] = caretColumn
    def pointerSpace(): Optional[String] = Optional.empty
    def sourcePath(): Optional[String] = Optional.empty
    def sourceFile(): Optional[File] = Optional.empty
    override def startLine(): Optional[Integer] = start.fold(Optional.empty)(s => some(s._1))
    override def startColumn(): Optional[Integer] = start.fold(Optional.empty)(s => some(s._2))
    override def endLine(): Optional[Integer] = end.fold(Optional.empty)(e => some(e._1))
    override def endColumn(): Optional[Integer] = end.fold(Optional.empty)(e => some(e._2))

  test("ranges convert zero-based columns to one-based") {
    assertEquals(
      Diagnostics.range(position(start = Some((4, 24)), end = Some((4, 26)))),
      Some(Range(Pos(4, 25), Pos(4, 27)))
    )
  }

  test("without a range, the caret becomes a one-character range") {
    assertEquals(Diagnostics.range(position(caretLine = some(3), caretColumn = some(0))), Some(Range(Pos(3, 1), Pos(3, 2))))
  }

  test("a bare line becomes the start of that line") {
    assertEquals(Diagnostics.range(position(caretLine = some(3))), Some(Range(Pos(3, 1), Pos(3, 1))))
  }

  test("global diagnostics have no range") {
    assertEquals(Diagnostics.range(position()), None)
  }

  test("numeric Scala 3 error codes are rendered as Ennn") {
    assertEquals(Diagnostics.errorCode("7"), Some("E007"))
    assertEquals(Diagnostics.errorCode("172"), Some("E172"))
    assertEquals(Diagnostics.errorCode("deprecation"), Some("deprecation"))
  }

  test("negative Scala 3 message ids mean no code") {
    assertEquals(Diagnostics.errorCode("-1"), None)
  }

  test("colours are stripped from messages") {
    assertEquals(Diagnostics.plain("Found:    (\u001b[32m42\u001b[0m : Int)"), "Found:    (42 : Int)")
  }

  test("forked javac's detail lines are recovered from the line content") {
    val forked = position(content = "    return q;\n           ^  symbol:   variable q\n  location: class Broken\n1 error\n")
    assertEquals(Diagnostics.javacDetails(forked), List("  symbol:   variable q", "  location: class Broken"))
  }

  test("a source line with ^ in it is not mistaken for a caret line") {
    assertEquals(Diagnostics.javacDetails(position(content = "int x = a ^ b;")), Nil)
  }

  test("javac's severity prefix is stripped, detail lines are kept") {
    assertEquals(
      Diagnostics.plain("error: cannot find symbol\n  symbol:   variable q"),
      "cannot find symbol\n  symbol:   variable q"
    )
  }
