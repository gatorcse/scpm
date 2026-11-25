package scpm.worker

import java.io.File
import java.util.Optional

class DiagnosticsSuite extends munit.FunSuite:
  private def some(i: Int): Optional[Integer] = Optional.of(i)

  private def position(
      caretLine: Optional[Integer] = Optional.empty,
      caretColumn: Optional[Integer] = Optional.empty,
      start: Option[(Int, Int)] = None,
      end: Option[(Int, Int)] = None
  ): xsbti.Position = new:
    def line(): Optional[Integer] = caretLine
    def lineContent(): String = ""
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

  test("without a range, the caret position is used") {
    assertEquals(Diagnostics.range(position(caretLine = some(3), caretColumn = some(0))), Some(Range(Pos(3, 1), Pos(3, 1))))
  }

  test("global diagnostics have no range") {
    assertEquals(Diagnostics.range(position()), None)
  }

  test("numeric Scala 3 error codes are rendered as Ennn") {
    assertEquals(Diagnostics.errorCode("7"), "E007")
    assertEquals(Diagnostics.errorCode("172"), "E172")
    assertEquals(Diagnostics.errorCode("deprecation"), "deprecation")
  }

  test("colours are stripped from messages") {
    assertEquals(Diagnostics.plain("Found:    (\u001b[32m42\u001b[0m : Int)"), "Found:    (42 : Int)")
  }
