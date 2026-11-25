package scpm.worker

import java.nio.file.Files

class ValidationSuite extends munit.FunSuite:
  private val jar = Files.createTempFile("scpm", ".jar").toString
  private val src = Files.createTempFile("scpm", ".scala")

  private val valid = CompileParams(
    target = "t",
    scalaVersion = "3.9.0",
    scalaCompilerClasspath = List(jar),
    scalaLibraryClasspath = List(jar),
    compilerBridge = jar,
    sources = List(src.toString),
    classesDir = src.getParent.resolve("scpm-out/classes").toString,
    analysisFile = src.getParent.resolve("scpm-out/analysis.zip").toString
  )

  test("a request with existing toolchain jars and sources is valid") {
    assertEquals(Validation.compile(valid), Nil)
  }

  test("missing toolchain jars and relative paths are reported by field") {
    val problems = Validation.compile(
      valid.copy(compilerBridge = "/nope/bridge.jar", classesDir = "out/classes", classpath = List("lib.jar"))
    )
    assertEquals(
      problems,
      List(
        "classesDir: not an absolute path: out/classes",
        "compilerBridge: no such file: /nope/bridge.jar",
        "classpath: not an absolute path: lib.jar"
      )
    )
  }

  test("sources inside classesDir are refused, since it is wiped") {
    val problems = Validation.compile(valid.copy(classesDir = src.getParent.toString))
    assert(problems.exists(_.contains("inside classesDir")), problems)
  }

  test("sources inside classesDir are refused however classesDir is spelled") {
    val dir = src.getParent
    val link = Files.createTempDirectory("scpm-link").resolve("out")
    Files.createSymbolicLink(link, dir)
    for classesDir <- List(dir.resolve("."), dir.resolve("sub/.."), link, dir.resolve(s"../${dir.getFileName}")) do
      val problems = Validation.compile(valid.copy(classesDir = classesDir.toString))
      assert(problems.exists(_.contains("inside classesDir")), s"$classesDir: $problems")
  }

  test("a classesDir next to the sources is fine, even before it exists") {
    val dir = Files.createTempDirectory("scpm-ws")
    val source = Files.createDirectories(dir.resolve("src")).resolve("A.scala")
    Files.writeString(source, "object A")
    val params = valid.copy(sources = List(source.toString), classesDir = dir.resolve("out/../outx/classes").toString)
    assertEquals(Validation.compile(params), Nil)
  }
