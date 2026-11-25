package scpm.worker

import java.nio.file.{ Files, InvalidPathException, Path }

/** Checks a request before it is queued, so path mistakes come back as `bad_request`. */
object Validation:
  def compile(p: CompileParams): List[String] =
    def absolute(field: String, value: String): Either[String, Path] =
      try
        val path = Path.of(value)
        Either.cond(path.isAbsolute, path, s"$field: not an absolute path: $value")
      catch case _: InvalidPathException => Left(s"$field: not a valid path: $value")
    def existing(field: String, value: String): Either[String, Path] =
      absolute(field, value).filterOrElse(Files.exists(_), s"$field: no such file: $value")
    def all(field: String, values: Seq[String], check: (String, String) => Either[String, Path]) =
      values.map(check(field, _))

    val classesDir = absolute("classesDir", p.classesDir)
    val sources = all("sources", p.sources, existing)
    val sourceInsideOutput =
      for
        dir <- classesDir.toOption.toList
        source <- sources.flatMap(_.toOption) if source.startsWith(dir)
      yield s"sources: $source is inside classesDir, which is wiped for full compiles"
    val checked =
      List(classesDir, absolute("analysisFile", p.analysisFile), existing("compilerBridge", p.compilerBridge)) ++
        all("scalaCompilerClasspath", p.scalaCompilerClasspath, existing) ++
        all("scalaLibraryClasspath", p.scalaLibraryClasspath, existing) ++
        sources ++
        all("classpath", p.classpath, absolute) ++
        all("javaHome", p.javaHome.toList, existing) ++
        all("rootPaths", p.rootPaths.values.toList, absolute) ++
        all("upstreamAnalyses.classesDir", p.upstreamAnalyses.map(_.classesDir), absolute) ++
        all("upstreamAnalyses.analysisFile", p.upstreamAnalyses.map(_.analysisFile), absolute)
    val emptyCompiler = Option.when(p.scalaCompilerClasspath.isEmpty)("scalaCompilerClasspath: empty")
    checked.flatMap(_.left.toOption) ++ sourceInsideOutput ++ emptyCompiler
