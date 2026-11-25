package scpm.worker

import java.io.IOException
import java.nio.file.{ Files, InvalidPathException, Path }
import scala.annotation.tailrec

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
        canonicalDir = canonical(dir)
        source <- sources.flatMap(_.toOption) if canonical(source).startsWith(canonicalDir)
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

  /**
   * `path` with symbolic links, `.` and `..` resolved, so that different spellings of one
   * location compare equal. A part that does not exist yet is only normalized.
   */
  private[worker] def canonical(path: Path): Path =
    @tailrec def resolve(existing: Path, rest: List[Path]): Path =
      if existing == null then path.normalize
      else if Files.exists(existing) then rest.foldLeft(existing.toRealPath())(_.resolve(_)).normalize
      else resolve(existing.getParent, existing.getFileName :: rest)
    try resolve(path, Nil)
    catch case _: IOException => path.normalize
