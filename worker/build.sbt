// The runtime dependencies of each helper live in deps/<helper>.txt so that the scpm CLI can
// resolve exactly the same classpath through coursier. This build reads them from there.
def runtimeDeps(helper: String): Seq[ModuleID] =
  IO.readLines(file(s"deps/$helper.txt")).map(_.trim).filter(_.nonEmpty).map { line =>
    line.split(':') match
      case Array(org, name, version) => org % name % version
      case _ => sys.error(s"deps/$helper.txt: expected org:name:version, got '$line'")
  }

val workerDeps = runtimeDeps("compile-worker")
def workerDepVersion(name: String): String =
  workerDeps.find(_.name == name).getOrElse(sys.error(s"deps/compile-worker.txt lacks $name")).revision

ThisBuild / organization := "dev.scpm"
ThisBuild / version := "0.1.0"
ThisBuild / scalaVersion := workerDepVersion("scala3-library_3")
ThisBuild / scalacOptions ++= Seq("-deprecation", "-feature", "-Wunused:all")

lazy val root = project
  .in(file("."))
  .aggregate(compileWorker, testRunner)
  .settings(publish / skip := true)

lazy val compileWorker = project
  .in(file("compile-worker"))
  .settings(
    name := "scpm-compile-worker",
    libraryDependencies ++= workerDeps ++ Seq(
      "com.github.plokhotnyuk.jsoniter-scala" %% "jsoniter-scala-macros" %
        workerDepVersion("jsoniter-scala-core_3") % "compile-internal",
      "org.scalameta" %% "munit" % "1.3.6" % Test
    )
  )

// Plain Java on purpose: the runner shares one flat classpath with the code under test, so it
// must not bring a Scala library of its own (see README).
lazy val testRunner = project
  .in(file("test-runner"))
  .settings(
    name := "scpm-test-runner",
    crossPaths := false,
    autoScalaLibrary := false,
    javacOptions ++= Seq("--release", "17", "-Xlint:all", "-Werror"),
    libraryDependencies ++= runtimeDeps("test-runner") ++ Seq(
      "com.github.sbt" % "junit-interface" % "0.13.3" % Test
    )
  )
