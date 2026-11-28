package scpm.resolver

import com.github.plokhotnyuk.jsoniter_scala.core.*
import com.github.plokhotnyuk.jsoniter_scala.macros.*

import java.io.File
import java.net.URI
import java.nio.file.Files

/** Files shared with scpm's Rust tests, in `worker/resolver/fixtures`. */
object Fixtures:
  def file(name: String): File =
    Iterator
      .iterate(File(".").getAbsoluteFile)(_.getParentFile)
      .takeWhile(_ != null)
      .flatMap(d => Seq(File(d, s"resolver/fixtures/$name"), File(d, s"worker/resolver/fixtures/$name")))
      .find(_.isFile)
      .getOrElse(sys.error(s"fixture $name not found"))

  final case class Repo(id: String, groups: Option[List[String]] = None)
  final case class GroupsCase(repositories: List[Repo], expect: Map[String, List[String]])
  final case class GroupsFile(cases: List[GroupsCase])
  given JsonValueCodec[GroupsFile] = JsonCodecMaker.make

  final case class NoProxyCase(noProxy: List[String], expect: Map[String, Boolean])
  final case class NoProxyFile(cases: List[NoProxyCase])
  given JsonValueCodec[NoProxyFile] = JsonCodecMaker.make

  def read[A: JsonValueCodec](name: String): A = readFromArray[A](Files.readAllBytes(file(name).toPath))

class GroupsSuite extends munit.FunSuite:
  test("the shared routing fixture") {
    for c <- Fixtures.read[Fixtures.GroupsFile]("groups.json").cases; (group, ids) <- c.expect do
      val repos = c.repositories.map(r => RepoSpec(r.id, s"https://${r.id}.example/", r.groups))
      assertEquals(Groups.allowed(repos, group).toList, ids, s"$group in ${c.repositories.map(_.id)}")
  }

class ProxiesSuite extends munit.FunSuite:
  test("the shared noProxy fixture") {
    for c <- Fixtures.read[Fixtures.NoProxyFile]("no-proxy.json").cases; (host, direct) <- c.expect do
      assertEquals(Proxies(ProxySpec(noProxy = c.noProxy)).bypasses(host), direct, s"$host with ${c.noProxy}")
  }

  test("schemes pick their proxy, and bypassed hosts go direct") {
    val p = Proxies(ProxySpec(https = Some(ProxyServer("proxy.example", 3128)), noProxy = List(".corp.example")))
    assertEquals(p.select(URI("https://repo1.maven.org/x")).get(0).address.toString, "proxy.example/<unresolved>:3128")
    assertEquals(p.select(URI("http://repo1.maven.org/x")).get(0), java.net.Proxy.NO_PROXY)
    assertEquals(p.select(URI("https://a.corp.example/x")).get(0), java.net.Proxy.NO_PROXY)
  }

class EnvironmentSuite extends munit.FunSuite:
  test("start-up refuses inherited coursier, Scala CLI and proxy settings") {
    assertEquals(Environment.check(Set("coursier.config-dir", "scala-cli.config", "user.home", "java.version")), None)
    for p <- Seq("coursier.cache", "coursier.mode", "scala-cli.home", "https.proxyHost", "http.proxyHost", "java.net.useSystemProxies") do
      assert(Environment.check(Set(p, "user.home")).exists(_.contains(p)), p)
  }

class MergeSuite extends munit.FunSuite:
  private def e(name: String, classifier: Option[String] = None): Entry =
    Entry("g", name, "1", classifier, "jar", s"https://r/$name", s"/c/$name", "central")

  private def keys(l: List[Entry]) = l.map(x => Merge.key(x) + x.systems.fold("")(s => s.mkString(" [", ",", "]")))

  test("entries on every system stay plain; the others name their systems") {
    val merged = Merge.merge(
      Map(
        "linux-x86_64" -> List(e("a"), e("n", Some("linux-x86_64")), e("b")),
        "macos-aarch64" -> List(e("a"), e("n", Some("osx-aarch_64")), e("b"))
      )
    )
    assertEquals(
      merged.map(keys),
      Right(List("g:a:1", "g:n:1:linux-x86_64 [linux-x86_64]", "g:n:1:osx-aarch_64 [macos-aarch64]", "g:b:1"))
    )
  }

  test("a jar-only classpath has no systems") {
    val l = List(e("a"), e("b"))
    assertEquals(Merge.merge(Map("linux-x86_64" -> l, "windows-x86_64" -> l)).map(_.map(_.systems)), Right(List(None, None)))
  }

  test("each system's projection is its own order, whatever order results arrive in") {
    val linux = List(e("a"), e("x"), e("b"), e("c"))
    val mac = List(e("a"), e("y"), e("c"))
    val win = List(e("z"), e("a"), e("b"), e("c"))
    val inputs = Seq("linux-x86_64" -> linux, "macos-aarch64" -> mac, "windows-x86_64" -> win)
    val results = inputs.permutations.map(p => Merge.merge(scala.collection.immutable.ListMap(p*))).toList.distinct
    assertEquals(results.size, 1, "the same bytes from any arrival order")
    val Right(merged) = results.head: @unchecked
    for (s, own) <- inputs do
      assertEquals(merged.filter(_.systems.forall(_.contains(s))).map(Merge.key), own.map(Merge.key), s)
  }

  test("orders that contradict each other are written per system") {
    val linux = List(e("a"), e("b"))
    val mac = List(e("b"), e("a"))
    val merged = Merge.merge(Map("linux-x86_64" -> linux, "macos-aarch64" -> mac))
    assertEquals(merged.left.map(_.view.mapValues(_.map(Merge.key)).toMap), Left(Map("linux-x86_64" -> List("g:a:1", "g:b:1"), "macos-aarch64" -> List("g:b:1", "g:a:1"))))
  }

  test("ties: earliest in the first system, absent ones after, then the smallest key") {
    val merged = Merge.merge(Map("linux-x86_64" -> List(e("m"), e("c")), "macos-aarch64" -> List(e("b"), e("a"))))
    assertEquals(merged.map(_.map(Merge.key)), Right(List("g:m:1", "g:c:1", "g:b:1", "g:a:1")))
  }

class IntervalSuite extends munit.FunSuite:
  test("Maven intervals") {
    import coursier.version.Version
    val r = Interval.parse("[4.1.0,4.2.0)")
    assert(r.contains(Version("4.1.115.Final")))
    assert(!r.contains(Version("4.2.0.Final")) || Version("4.2.0.Final") < Version("4.2.0"))
    assert(Interval.parse("[3.3.0,)").contains(Version("3.3.6")))
    assert(!Interval.parse("[3.3.0,)").contains(Version("3.2.3")))
  }

/**
 * The sidecar builds each coursier object with every defaulted field given (design §5.2.1, "No
 * ambient configuration"): a default the sidecar doesn't set may read the environment. These are
 * the numbers of fields `Service` names; a coursier upgrade that adds one fails here, until the
 * new field is set there too and the number below raised.
 */
class DefaultedFieldsSuite extends munit.FunSuite:
  private def fields(c: Class[?]): Int = c.getConstructors.map(_.getParameterCount).max

  // Implicit `Sync` parameters are one more constructor parameter.
  test("every field of the objects the sidecar builds is set") {
    assertEquals(fields(classOf[coursier.cache.FileCache[?]]), 32 + 1, "FileCache")
    assertEquals(fields(classOf[coursier.Resolve[?]]), 16 + 1, "Resolve")
    assertEquals(fields(classOf[coursier.Artifacts[?]]), 12 + 1, "Artifacts")
    assertEquals(fields(classOf[coursier.Versions[?]]), 5 + 1, "Versions")
    assertEquals(fields(classOf[coursier.Fetch[?]]), 3, "Fetch")
    assertEquals(fields(classOf[coursier.params.ResolutionParams]), 25, "ResolutionParams")
    assertEquals(fields(classOf[coursier.credentials.DirectCredentials]), 8, "DirectCredentials")
    assertEquals(fields(classOf[coursier.maven.MavenRepository]), 5, "MavenRepository")
    assertEquals(fields(classOf[coursier.core.BomDependency]), 4, "BomDependency")
    assertEquals(
      coursier.cache.HostThrottle.getClass.getMethods.filter(_.getName == "apply").map(_.getParameterCount).max,
      5,
      "HostThrottle.apply"
    )
  }

class RecordingSuite extends munit.FunSuite:
  test("checksum errors give expected and actual; the cache's own misses aren't a repository's") {
    assertEquals(
      Recording.checksums("wrong checksum: /c/x.jar (expected SHA-1 abc123 in /c/x.jar.sha1, got def456)"),
      ("abc123", "def456")
    )
    assertEquals(Recording.checksums("checksum not found: /c/x.jar"), ("", ""))
    assert(Recording.localMiss("/cache/https/h/x.pom not found", "/cache"))
    assert(Recording.localMiss("file in cache not found or too old: /cache/x", "/cache"))
    assert(!Recording.localMiss("not found: https://h/x.pom", "/cache"))
    assert(!Recording.localMiss("unauthorized: https://h/x.pom (R)", "/cache"))
  }

class TrustSuite extends munit.FunSuite:
  // Certificates stand in as names: only which stores are combined matters here.
  private val (root, user, jdk) = (Seq("root"), Seq("user"), Seq("jdk"))
  private def stores(have: Map[String, Seq[String]]): String => Option[Seq[String]] = have.get
  private val noPem: String => Option[Seq[String]] = _ => None

  test("macOS: the Keychain's roots, or without KeychainStore-ROOT (JDK 17–22), cacerts") {
    val withRoot = Trust.select("Mac OS X", stores(Map("KeychainStore-ROOT" -> root, "KeychainStore" -> user)), None, noPem, jdk)
    assertEquals(withRoot, (root ++ user, None))
    val (certs, note) = Trust.select("Mac OS X", stores(Map("KeychainStore" -> user)), None, noPem, jdk)
    assertEquals(certs, jdk ++ user)
    assert(note.exists(_.contains("KeychainStore-ROOT")), note)
  }

  test("Windows: Windows-ROOT and cacerts, since Windows fills its roots lazily") {
    assertEquals(Trust.select("Windows 11", stores(Map("Windows-ROOT" -> root)), None, noPem, jdk), (root ++ jdk, None))
  }

  test("Linux: the PEM bundle, else cacerts with a note") {
    assertEquals(Trust.select("Linux", stores(Map.empty), Some("/b.pem"), _ => Some(root), jdk), (root, None))
    val (certs, note) = Trust.select("Linux", stores(Map.empty), Some("/nonexistent/bundle.pem"), noPem, jdk)
    assertEquals(certs, jdk)
    assert(note.exists(_.contains("PEM bundle")), note)
  }
