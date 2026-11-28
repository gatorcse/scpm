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
  /** Every request's path and `Authorization` header. */
  val headers = ConcurrentLinkedQueue[(String, Option[String])]()
  /** Paths answered with a status instead of a file. */
  val statuses = scala.collection.concurrent.TrieMap.empty[String, Int]
  /** Paths answered with a redirect to a URL. */
  val redirects = scala.collection.concurrent.TrieMap.empty[String, String]
  /** Paths under a prefix that need basic auth for a user and password, challenging with a realm. */
  @volatile var protect: Option[(String, String, String, String)] = None
  private val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
  server.createContext(
    "/",
    exchange =>
      val path = exchange.getRequestURI.getPath
      val auth = Option(exchange.getRequestHeaders.getFirst("Authorization"))
      requests.add(path)
      headers.add(path -> auth)
      val expected = protect.collect {
        case (prefix, user, password, realm) if path.startsWith(prefix) =>
          ("Basic " + java.util.Base64.getEncoder.encodeToString(s"$user:$password".getBytes), realm)
      }
      (redirects.get(path), statuses.get(path), expected) match
        case (Some(to), _, _) =>
          exchange.getResponseHeaders.add("Location", to)
          exchange.sendResponseHeaders(302, -1)
        case (_, Some(status), _) => exchange.sendResponseHeaders(status, -1)
        case (_, _, Some((header, realm))) if !auth.contains(header) =>
          exchange.getResponseHeaders.add("WWW-Authenticate", s"""Basic realm="$realm"""")
          exchange.sendResponseHeaders(401, -1)
        case _ =>
          files.synchronized(files.get(path)) match
            case Some(body) =>
              exchange.sendResponseHeaders(200, body.length.toLong)
              exchange.getResponseBody.write(body)
            case None => exchange.sendResponseHeaders(404, -1)
      exchange.close()
  )
  server.start()

  def origin: String = s"http://127.0.0.1:${server.getAddress.getPort}"
  def url(repo: String): String = s"$origin/$repo/"

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

  /** Serves `body` at `path`. */
  def put(path: String, body: Array[Byte]): Unit = files.synchronized(files(path) = body)

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

  test("a mapped native jar the repository answers 404 for is left out, not an error") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val wrong = List(NativeSpec("org.fix:nat", None, Map("linux-x86_64" -> "linux-riscv64")))
    val Seq(r) = resolve(cache, request(List("linux-x86_64"), natives = wrong)): @unchecked
    assertEquals(keys(r), List("org.fix:app:1.0", "org.fix:base:1.0", "org.fix:nat:1.0"))
  }

  // Which answers coursier 2.1.27 skips an optional artifact on, measured here: 404 and 403 (S3
  // answers 403 for a missing object without ListBucket); a 401 still fails the resolution.
  test("a mapped native jar answered 403 is left out too; 401 fails") {
    val path = "/public/org/fix/nat/1.0/nat-1.0-linux-x86_64.jar"
    try
      server.statuses.put(path, 403)
      val Seq(forbidden) = resolve(Files.createTempDirectory("c").toFile, request(List("linux-x86_64"))): @unchecked
      assertEquals(forbidden.error, None, forbidden.toString)
      assert(!keys(forbidden).exists(_.contains("nat:1.0:linux")), forbidden.toString)
      server.statuses.put(path, 401)
      val Seq(unauthorized) = resolve(Files.createTempDirectory("c").toFile, request(List("linux-x86_64"))): @unchecked
      assertEquals(unauthorized.error.map(_.code), Some("unauthorized"), unauthorized.toString)
    finally server.statuses.remove(path)
  }

  test("several rows for one module: the first that maps the system at the version") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val rows = List(
      NativeSpec("org.fix:nat", Some("[0.1,1.0)"), Map("linux-x86_64" -> "linux-riscv64")),
      NativeSpec("org.fix:nat", Some("[1.0,)"), Map("macos-aarch64" -> "osx-aarch_64")),
      NativeSpec("org.fix:nat", Some("[1.0,)"), Map("linux-x86_64" -> "linux-x86_64"))
    )
    val Seq(r) = resolve(cache, request(List("linux-x86_64", "macos-aarch64"), natives = rows)): @unchecked
    assertEquals(
      keys(r).filter(_.contains("nat")),
      List("org.fix:nat:1.0", "org.fix:nat:1.0:linux-x86_64 [linux-x86_64]", "org.fix:nat:1.0:osx-aarch_64 [macos-aarch64]")
    )
  }

  test("listVersions over repositories sharing one URL names the one allowed for the group") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    // `central` and `acme` (com.acme only) both reached through the acme URL, as a mirror would.
    val shared = List(RepoSpec("central", server.url("acme")), RepoSpec("acme", server.url("acme"), Some(List("com.acme"))))
    val r = service.listVersions(ListVersionsParams(shared, cache.getPath, module = "com.acme:lib"))
    assertEquals(r.versions, List("0.9", "1.0"))
    assertEquals(r.repositories, List(RepoVersions("acme", List("0.9", "1.0"))))
  }

  test("listVersions: a refused directory listing is no listing; refused metadata is an error") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    // No maven-metadata.xml for org.fix:app; coursier then tries the directory, which is refused.
    server.statuses.put("/public/org/fix/app/", 403)
    val r = service.listVersions(ListVersionsParams(repos, cache.getPath, module = "org.fix:app"))
    assertEquals(r.versions, Nil)
    server.statuses.remove("/public/org/fix/app/")
    server.statuses.put("/public/org/fix/base/maven-metadata.xml", 401)
    val err = intercept[ResolverError](service.listVersions(ListVersionsParams(repos, Files.createTempDirectory("c").toFile.getPath, module = "org.fix:base")))
    assertEquals(err.body.code, "unauthorized")
    server.statuses.remove("/public/org/fix/base/maven-metadata.xml")
  }

  test("a refused metadata refresh is an error, not the stale cached versions") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    // TTL 0: every listing re-checks maven-metadata.xml (a `LocalUpdateChanging` pass).
    val params = ListVersionsParams(repos, cache.getPath, ttl = Some(0), module = "org.fix:base")
    assertEquals(service.listVersions(params).versions, List("0.1", "1.0"))
    server.metadata("public", "org.fix", "base", Seq("0.1", "1.0", "2.0"))
    server.statuses.put("/public/org/fix/base/maven-metadata.xml", 401)
    try
      val err = intercept[ResolverError](service.listVersions(params))
      assertEquals(err.body.code, "unauthorized")
    finally
      server.statuses.remove("/public/org/fix/base/maven-metadata.xml")
      server.metadata("public", "org.fix", "base", Seq("0.1", "1.0"))
  }

  // A served checksum that doesn't match is `checksum`, never a quiet miss: the error names the
  // cached file, which must not make it look like the cache's own miss. With the cache's
  // canonical path and through a symlink (coursier writes the path as given).
  for (kind, cacheDir) <- Seq(
      "canonical" -> (() => Files.createTempDirectory("resolver-cache").toRealPath().toFile),
      "symlinked" -> { () =>
        val real = Files.createTempDirectory("resolver-cache-real").toRealPath()
        val link = Files.createTempDirectory("resolver-cache-links").toRealPath().resolve("cache")
        Files.createSymbolicLink(link, real).toFile
      }
    )
  do
    test(s"a bad served checksum is `checksum`, in resolve and listVersions ($kind cache)") {
      server.module("public", "org.fix", "badsum", "1.0")
      server.put("/public/org/fix/badsum/1.0/badsum-1.0.pom.sha1", "1234567890abcdef1234567890abcdef12345678".getBytes)
      val Seq(r) = resolve(cacheDir(), request(List("linux-x86_64"), root = "org.fix:badsum", natives = Nil)): @unchecked
      val err = r.error.getOrElse(fail(s"expected an error: $r"))
      assertEquals(err.code, "checksum", err.toString)
      val data = new String(com.github.plokhotnyuk.jsoniter_scala.core.writeToArray(err.data.get), "UTF-8")
      assert(data.contains("\"expected\":\"1234567890abcdef1234567890abcdef12345678\""), s"$data ${err.message}")
      assert(data.contains("\"actual\":\"38a"), s"$data ${err.message}")

      server.metadata("public", "org.fix", "badmeta", Seq("1.0"))
      server.put("/public/org/fix/badmeta/maven-metadata.xml.sha1", ("0" * 40).getBytes)
      val e = intercept[ResolverError](service.listVersions(ListVersionsParams(repos, cacheDir().getPath, module = "org.fix:badmeta")))
      assertEquals(e.body.code, "checksum")
    }

  test("an offline miss names the POM it would have needed") {
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val params = ResolveParams(repos, cache.getPath, offline = true, requests = List(request(List("linux-x86_64"))))
    val err = service.resolve(params).results.head.error.getOrElse(fail("expected an error"))
    assertEquals(err.code, "offline_miss")
    val data = new String(com.github.plokhotnyuk.jsoniter_scala.core.writeToArray(err.data.get), "UTF-8")
    assert(data.contains(server.url("public") + "org/fix/app/1.0/app-1.0.pom"), data)
  }

  test("the file cache is built with the sealed settings") {
    val cache = service.fileCache(CacheSpec(repos, "/tmp/c", None, refresh = false, offline = false))
    assertEquals(cache.allowCacheSubstitution, false)
    assertEquals(cache.followHttpsToHttpRedirections, false)
    assertEquals(cache.checksums, Seq(Some("SHA-1"), None))
    assertEquals(cache.cachePolicies, Seq(coursier.cache.CachePolicy.LocalUpdateChanging, coursier.cache.CachePolicy.FetchMissing))
    assertEquals(cache.userAgent, Some("scpm-test"))
    assertEquals(cache.credentials, Nil)
    assert(cache.sslSocketFactoryOpt.isDefined)
    val params = Service.resolutionParams(Systems.osInfo("linux-x86_64"), "25", Map.empty)
    assertEquals((params.useSystemOsInfo, params.useSystemJdkVersion), (false, false))
    assertEquals(params.osInfoOpt.flatMap(_.name), Some("linux"))
  }

/** Credentials: scoped to their repository's origin, realm-scoped ones only after a challenge
  * naming the realm, and never forwarded on a redirect to another origin. */
class CredentialsSuite extends munit.FunSuite:
  private val secure = FixtureServer()
  private val open = FixtureServer()
  private val service = Service(None, None, Some("scpm-test"))
  private val basic = "Basic " + java.util.Base64.getEncoder.encodeToString("alice:pw".getBytes)
  secure.protect = Some(("/secure/", "alice", "pw", "R"))
  secure.module("secure", "com.sec", "lib", "1.0", deps = Seq("org.fix:base:1.0"))
  open.module("public", "org.fix", "base", "1.0")
  open.module("public", "com.sec", "red", "1.0")
  for ext <- Seq("pom", "jar") do
    secure.redirects.put(s"/secure/com/sec/red/1.0/red-1.0.$ext", s"${open.url("public")}com/sec/red/1.0/red-1.0.$ext")

  override def afterAll(): Unit =
    secure.stop()
    open.stop()

  private def run(root: String, auth: AuthSpec) =
    val cache = Files.createTempDirectory("resolver-cache").toFile
    val repos = List(RepoSpec("secure", secure.url("secure"), auth = Some(auth)), RepoSpec("public", open.url("public")))
    val req = ResolveRequestSpec("t", List("linux-x86_64"), "25", roots = List(RootSpec(root, "1.0")), artifactTypes = List("jar"))
    service.resolve(ResolveParams(repos, cache.getPath, requests = List(req))).results.head

  private def clear(): Unit =
    secure.headers.clear()
    open.headers.clear()

  test("credentials reach their repository's origin only") {
    clear()
    val r = run("com.sec:lib", AuthSpec("alice", "pw"))
    assert(r.error.isEmpty, r.toString)
    assert(secure.headers.asScala.exists(_._2.contains(basic)))
    assert(open.headers.asScala.forall(_._2.isEmpty), open.headers.toString)
  }

  test("wrong credentials are unauthorized") {
    val r = run("com.sec:lib", AuthSpec("alice", "nope"))
    assertEquals(r.error.map(_.code), Some("unauthorized"), r.toString)
  }

  test("realm-scoped credentials go only after a challenge naming that realm") {
    clear()
    val r = run("com.sec:lib", AuthSpec("alice", "pw", Some("R")))
    assert(r.error.isEmpty, r.toString)
    val pom = secure.headers.asScala.toList.filter(_._1.endsWith("lib-1.0.pom")).map(_._2)
    assertEquals(pom.headOption, Some(None), "first anonymous")
    assert(pom.contains(Some(basic)), s"then with the credentials: $pom")
    clear()
    val other = run("com.sec:lib", AuthSpec("alice", "pw", Some("Other")))
    assertEquals(other.error.map(_.code), Some("unauthorized"), other.toString)
    assert(secure.headers.asScala.forall(_._2.isEmpty), "never sent for another realm")
  }

  test("credentials are not forwarded on a redirect to another origin") {
    clear()
    val r = run("com.sec:red", AuthSpec("alice", "pw"))
    assert(r.error.isEmpty, r.toString)
    assert(open.headers.asScala.exists(_._1.endsWith("red-1.0.pom")), open.headers.toString)
    assert(open.headers.asScala.forall(_._2.isEmpty), open.headers.toString)
  }
