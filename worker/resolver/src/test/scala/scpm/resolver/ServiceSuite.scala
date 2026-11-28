package scpm.resolver

import com.sun.net.httpserver.HttpServer

import java.net.InetSocketAddress
import java.nio.file.Files
import java.util.concurrent.ConcurrentLinkedQueue
import scala.concurrent.duration.*
import scala.concurrent.{ Await, Future }
import scala.concurrent.ExecutionContext.Implicits.global
import scala.jdk.CollectionConverters.*

/**
 * A local Maven server with two repositories (`/public/` and `/acme/`, the latter for `com.acme`
 * only), recording every request, for resolutions with routing, systems and native mappings.
 */
final class FixtureServer:
  private val files = scala.collection.mutable.Map.empty[String, Array[Byte]]
  val requests = ConcurrentLinkedQueue[String]()
  private val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
  server.createContext(
    "/",
    exchange =>
      val path = exchange.getRequestURI.getPath
      requests.add(path)
      files.synchronized(files.get(path)) match
        case Some(body) =>
          exchange.sendResponseHeaders(200, body.length.toLong)
          exchange.getResponseBody.write(body)
        case None => exchange.sendResponseHeaders(404, -1)
      exchange.close()
  )
  server.start()

  def url(repo: String): String = s"http://127.0.0.1:${server.getAddress.getPort}/$repo/"

  /** A module with a POM (`deps` as `org:name:version`, `profiles` as (OS family, deps)) and jars
    * for the given classifiers ("" is the main jar). */
  def module(
      repo: String,
      org: String,
      name: String,
      version: String,
      deps: Seq[String] = Nil,
      profiles: Seq[(String, Seq[String])] = Nil,
      parent: Option[String] = None,
      classifiers: Seq[String] = Seq(""),
      packaging: String = "jar"
  ): Unit =
    def dep(d: String) =
      val Array(o, n, v) = d.split(':')
      s"<dependency><groupId>$o</groupId><artifactId>$n</artifactId><version>$v</version></dependency>"
    val parentXml = parent.fold("") { p =>
      val Array(o, n, v) = p.split(':')
      s"<parent><groupId>$o</groupId><artifactId>$n</artifactId><version>$v</version></parent>"
    }
    val profileXml = profiles.zipWithIndex.map { case ((family, ds), i) =>
      s"<profile><id>p$i</id><activation><os><family>$family</family></os></activation><dependencies>${ds.map(dep).mkString}</dependencies></profile>"
    }.mkString
    val pom =
      s"""<?xml version="1.0"?><project><modelVersion>4.0.0</modelVersion>$parentXml<groupId>$org</groupId><artifactId>$name</artifactId><version>$version</version><packaging>$packaging</packaging><dependencies>${deps.map(dep).mkString}</dependencies><profiles>$profileXml</profiles></project>"""
    val base = s"/$repo/${org.replace('.', '/')}/$name/$version/$name-$version"
    files.synchronized {
      files(s"$base.pom") = pom.getBytes
      if packaging != "pom" then
        for c <- classifiers do files(base + (if c.isEmpty then "" else s"-$c") + ".jar") = s"$name $c".getBytes
    }

  def metadata(repo: String, org: String, name: String, versions: Seq[String]): Unit =
    val xml =
      s"<metadata><groupId>$org</groupId><artifactId>$name</artifactId><versioning><versions>${versions.map(v => s"<version>$v</version>").mkString}</versions></versioning></metadata>"
    files.synchronized(files(s"/$repo/${org.replace('.', '/')}/$name/maven-metadata.xml") = xml.getBytes)

  def stop(): Unit = server.stop(0)

class ServiceSuite extends munit.FunSuite:
  private val server = FixtureServer()
  private val service = Service(None, None, Some("scpm-test"))
  private val repos = List(RepoSpec("public", server.url("public")), RepoSpec("acme", server.url("acme"), Some(List("com.acme"))))

  // org.fix:app's POM pulls in winonly on Windows only; nat has native classifier jars.
  server.module("public", "org.fix", "base", "1.0")
  server.module("public", "org.fix", "winonly", "1.0")
  server.module("public", "org.fix", "nat", "1.0", classifiers = Seq("", "linux-x86_64", "osx-aarch_64"))
  server.module("public", "org.fix", "app", "1.0", deps = Seq("org.fix:base:1.0", "org.fix:nat:1.0"), profiles = Seq("windows" -> Seq("org.fix:winonly:1.0")))
  server.module("public", "org.fix", "parent", "1.0", packaging = "pom")
  server.module("acme", "com.acme", "lib", "1.0", deps = Seq("org.fix:base:1.0"), parent = Some("org.fix:parent:1.0"))
  server.metadata("acme", "com.acme", "lib", Seq("0.9", "1.0"))
  server.metadata("public", "org.fix", "base", Seq("0.1", "1.0"))

  override def afterAll(): Unit = server.stop()

  private val natives = List(NativeSpec("org.fix:nat", Some("[1.0,)"), Map("linux-x86_64" -> "linux-x86_64", "macos-aarch64" -> "osx-aarch_64")))

  private def request(systems: List[String], root: String = "org.fix:app", natives: List[NativeSpec] = natives) =
    ResolveRequestSpec(
      label = "test",
      systems = systems,
      jdk = "25",
      natives = natives,
      roots = List(RootSpec(root, "1.0")),
      artifactTypes = List("jar", "bundle", "test-jar")
    )

  private def resolve(cache: java.io.File, reqs: ResolveRequestSpec*): List[RequestResult] =
    service.resolve(ResolveParams(repos, cache.getPath, requests = reqs.toList)).results

  private def keys(r: RequestResult): List[String] =
    r.classpath.getOrElse(fail(s"no classpath: $r")).map(e => Merge.key(e) + e.systems.fold("")(s => s.mkString(" [", ",", "]")))

  private val three = List("linux-x86_64", "macos-aarch64", "windows-x86_64")

  test("one request resolves every system, with natives and OS profiles per system") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val Seq(r) = resolve(cache, request(three)): @unchecked
    assertEquals(
      keys(r),
      // Windows' order puts the profile's dependency before base, so the merge does too.
      List(
        "org.fix:app:1.0",
        "org.fix:winonly:1.0 [windows-x86_64]",
        "org.fix:base:1.0",
        "org.fix:nat:1.0",
        "org.fix:nat:1.0:linux-x86_64 [linux-x86_64]",
        "org.fix:nat:1.0:osx-aarch_64 [macos-aarch64]"
      )
    )
    assert(r.classpath.get.forall(_.repository == "public"))
    // Each system's projection equals a resolve for that system alone.
    for s <- three do
      val Seq(alone) = resolve(cache, request(List(s))): @unchecked
      val projection = r.classpath.get.filter(_.systems.forall(_.contains(s))).map(Merge.key)
      assertEquals(alone.classpath.get.map(Merge.key), projection, s)
      assert(alone.classpath.get.forall(_.systems.isEmpty))
  }

  test("requests with different system sets run concurrently with the results they get alone") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val a = request(List("linux-x86_64", "windows-x86_64"))
    val b = request(List("macos-aarch64"), natives = Nil)
    val alone = Seq(a, b).map(r => resolve(cache, r).head)
    val together = Await.result(Future.sequence(Seq(Future(resolve(cache, a).head), Future(resolve(cache, b).head))), 1.minute)
    assertEquals(together.map(keys), alone.map(keys))
    assertEquals(resolve(cache, a, b).map(keys), alone.map(keys), "one call with both")
  }

  test("groups route both ways: no repository is asked about a module it may not serve") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    server.requests.clear()
    val Seq(r) = resolve(cache, request(List("linux-x86_64"), root = "com.acme:lib", natives = Nil)): @unchecked
    assertEquals(keys(r), List("com.acme:lib:1.0", "org.fix:base:1.0"))
    assertEquals(r.classpath.get.map(_.repository), List("acme", "public"))
    val seen = server.requests.asScala.toList
    assert(seen.exists(_.startsWith("/acme/com/acme/")), seen)
    assert(!seen.exists(p => p.startsWith("/acme/") && !p.startsWith("/acme/com/acme/")), s"acme asked about a public name: $seen")
    assert(!seen.exists(_.startsWith("/public/com/acme")), s"public asked about an internal name: $seen")
  }

  test("listVersions asks only the allowed repositories") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    server.requests.clear()
    val r = service.listVersions(ListVersionsParams(repos, cache.getPath, module = "com.acme:lib"))
    assertEquals(r.versions, List("0.9", "1.0"))
    assertEquals(r.repositories, List(RepoVersions("acme", List("0.9", "1.0"))))
    assert(!server.requests.asScala.exists(_.startsWith("/public/")), server.requests)
    val pub = service.listVersions(ListVersionsParams(repos, cache.getPath, module = "org.fix:base"))
    assertEquals(pub.versions, List("0.1", "1.0"))
    assert(!server.requests.asScala.exists(_.startsWith("/acme/org/")), server.requests)
  }

  test("a missing module is not_found, naming what was tried") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val Seq(r) = resolve(cache, request(List("linux-x86_64"), root = "org.fix:missing", natives = Nil)): @unchecked
    val err = r.error.getOrElse(fail(s"expected an error: $r"))
    assertEquals(err.code, "not_found")
    val data = new String(com.github.plokhotnyuk.jsoniter_scala.core.writeToArray(err.data.get), "UTF-8")
    assert(data.contains("\"repository\":\"public\"") && data.contains("\"status\":\"404\""), data)
  }

  test("a mapped native jar that doesn't exist is an error") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val wrong = List(NativeSpec("org.fix:nat", None, Map("linux-x86_64" -> "linux-riscv64")))
    val Seq(r) = resolve(cache, request(List("linux-x86_64"), natives = wrong)): @unchecked
    assertEquals(r.error.map(_.code), Some("not_found"))
  }
