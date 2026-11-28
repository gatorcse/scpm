package scpm.resolver

import coursier.{ Artifacts, Fetch, Resolve, Versions }
import coursier.cache.{ ArtifactError, Cache, CacheLogger, CachePolicy, FileCache, HostThrottle }
import coursier.core.{
  Activation,
  BomDependency,
  Classifier,
  Configuration,
  Dependency,
  MinimizedExclusions,
  Module,
  ModuleName,
  Organization,
  Publication,
  Type,
  VariantPublication
}
import coursier.credentials.{ DirectCredentials, Password }
import coursier.error.{ CoursierError, FetchError, ResolutionError }
import coursier.maven.MavenRepository
import coursier.params.ResolutionParams
import coursier.util.{ Artifact, EitherT, Task }
import coursier.version.{ Version, VersionConstraint }

import java.io.File
import java.net.{ ProxySelector, URI }
import java.time.Clock
import java.util.Locale
import java.util.concurrent.{ ConcurrentLinkedQueue, ExecutorService, Executors }
import javax.net.ssl.SSLSocketFactory
import scala.concurrent.duration.*
import scala.concurrent.{ Await, ExecutionContext, Future }
import scala.jdk.CollectionConverters.*
import scala.util.control.NonFatal

/** The JVM properties a system reports (design §5.2.1, "Resolution context"). */
object Systems:
  val known: Map[String, Map[String, String]] = Map(
    "linux-x86_64" -> props("Linux", "amd64", ":"),
    "linux-aarch64" -> props("Linux", "aarch64", ":"),
    "macos-aarch64" -> props("Mac OS X", "aarch64", ":"),
    "macos-x86_64" -> props("Mac OS X", "x86_64", ":"),
    "windows-x86_64" -> props("Windows 11", "amd64", ";"),
    "windows-aarch64" -> props("Windows 11", "aarch64", ";")
  )

  // `os.version` is left unset, so profiles that test a version never activate.
  private def props(name: String, arch: String, sep: String) =
    Map("os.name" -> name, "os.arch" -> arch, "path.separator" -> sep)

  def osInfo(system: String): Activation.Os =
    Activation.Os.fromProperties(
      known.getOrElse(system, throw ResolverError.plain("bad_request", s"unknown system `$system`"))
    )

/** A Maven interval such as `[4.1.0,)` or `[1.0,2.0)`; absent bounds are open. */
final case class Interval(from: Option[(Version, Boolean)], to: Option[(Version, Boolean)]):
  def contains(v: Version): Boolean =
    from.forall((f, inclusive) => if inclusive then v >= f else v > f) &&
      to.forall((t, inclusive) => if inclusive then v <= t else v < t)

object Interval:
  def parse(s: String): Interval =
    val t = s.trim
    if t.length < 2 || !"[(".contains(t.head) || !"])".contains(t.last) || !t.contains(',') then
      throw ResolverError.plain("bad_request", s"invalid version range `$s`")
    val (lo, hi) = t.substring(1, t.length - 1).span(_ != ',')
    def bound(v: String, inclusive: Boolean) = Option(v.trim).filter(_.nonEmpty).map(x => (Version(x), inclusive))
    Interval(bound(lo, t.head == '['), bound(hi.drop(1), t.last == ']'))

/**
 * Records every URL a cache fetches during resolution and how it went, for errors' `tried` lists
 * and `listVersions`' status checks (design §5.2.1, "Errors").
 */
final class Recording(underlying: FileCache[Task]) extends Cache[Task]:
  val log = ConcurrentLinkedQueue[(String, Either[String, Unit])]()

  private val cacheRoots =
    val l = underlying.location
    Seq(l.getPath, l.getAbsolutePath, l.getCanonicalPath).distinct

  private def recorded(f: Cache.Fetch[Task]): Cache.Fetch[Task] = artifact =>
    EitherT(f(artifact).run.map { result =>
      if !result.left.exists(Recording.localMiss(_, cacheRoots)) then log.add(artifact.url -> result.map(_ => ()))
      result
    })

  // Every pass is recorded, since `LocalUpdateChanging` and `LocalUpdate` do ask the repository
  // about changing files (`maven-metadata.xml`) once their TTL has expired; only answers that
  // are the cache's own misses, not a repository's, are left out.
  // Every policy in turn, as `FileCache.fetch` does (`Versions` uses this one).
  override def fetch: Cache.Fetch[Task] = artifact =>
    fetchs.tail.foldLeft(fetchs.head(artifact))((acc, f) => acc.orElse(f(artifact)))
  override def fetchs: Seq[Cache.Fetch[Task]] =
    underlying.fetchs.map(recorded)
  override def ec: ExecutionContext = underlying.ec
  override def file(artifact: Artifact): EitherT[Task, ArtifactError, File] = underlying.file(artifact)
  override def loggerOpt: Option[CacheLogger] = underlying.loggerOpt

  def entries: List[(String, Either[String, Unit])] = log.asScala.toList

object Recording:
  /** Whether an error is the cache's own miss, which no repository answered. By kind, never by
    * whether the text names a cached file (a wrong checksum names the cached file too, and must
    * be kept): `not found: <cached path>`, `<cached path> not found`, or `file in cache not
    * found or too old`. `cacheRoots` are the cache's spellings (as given, absolute, canonical):
    * coursier writes the path as given, which a symlink makes differ from the canonical one. */
  def localMiss(error: String, cacheRoots: Seq[String]): Boolean =
    val e = error.trim
    def underCache(path: String) = cacheRoots.exists(r => path.trim.startsWith(r))
    e.startsWith("file in cache not found or too old") ||
      (e.startsWith("not found: ") && underCache(e.stripPrefix("not found: "))) ||
      (e.endsWith(" not found") && underCache(e.stripSuffix(" not found")))

  /** `(expected, actual)` from a checksum error's text, when it has them (both empty otherwise). */
  def checksums(error: String): (String, String) =
    // `expected [<type>] <hex> …, got <hex>`: the digests are the hex runs after each word.
    val expected = """expected (?:[A-Za-z0-9-]*[A-Za-z][A-Za-z0-9-]* )?([0-9a-fA-F]+)""".r
    val actual = """got ([0-9a-fA-F]+)""".r
    (expected.findFirstMatchIn(error), actual.findFirstMatchIn(error)) match
      // Coursier prints the expected digest as a number, dropping leading zeros: pad it back to
      // the length of the actual one.
      case (Some(e), Some(a)) =>
        val actual = a.group(1)
        (e.group(1).reverse.padTo(actual.length, '0').reverse, actual)
      case _                  => ("", "")

  /** An HTTP status, or what went wrong, from coursier's error text (`ArtifactError.describe`). */
  def status(error: String): String =
    val e = error.toLowerCase(Locale.ROOT)
    if e.startsWith("not found") then "404"
    else if e.startsWith("unauthorized") then "401"
    else if e.startsWith("forbidden") then "403"
    else if e.startsWith("file in cache not found or too old") then "offline"
    else if e.contains("checksum") then "checksum"
    else "transport"

/**
 * What the sidecar does once initialized: every coursier object is built with each defaulted
 * field given (design §5.2.1, "No ambient configuration"), so no default reads the environment,
 * the system properties or a configuration file.
 */
final class Service(proxy: Option[ProxySpec], trust: Option[TrustSpec], userAgent: Option[String]):
  ProxySelector.setDefault(Proxies(proxy.getOrElse(ProxySpec(noProxy = List("localhost", "127.0.0.1", "::1")))))
  private val sslFactory: SSLSocketFactory = Trust.socketFactory(trust)
  private val pool: ExecutorService = Service.daemonPool(Service.PoolSize, "scpm-resolver-download")
  // Bounded orchestration: a request's thread waits for its systems' resolutions, and those wait
  // for downloads, so each level has its own fixed pool (sharing one would deadlock once full),
  // sized to the CPUs; downloads run on `pool`.
  private val requests: ExecutionContext =
    ExecutionContext.fromExecutorService(Service.daemonPool(Service.Orchestration, "scpm-resolver-request"))
  private val systemsEc: ExecutionContext =
    ExecutionContext.fromExecutorService(Service.daemonPool(Service.Orchestration, "scpm-resolver-system"))
  // One throttle for the whole sidecar: a rate limit applies to the client as a whole.
  private val throttle: HostThrottle = HostThrottle(
    initialDelay = 1.second,
    maxDelay = Some(1.minute),
    delayMultiplier = 2.0,
    maxRetryAfter = Some(30.seconds),
    clock = Clock.systemDefaultZone()
  )

  def resolve(params: ResolveParams): ResolveResult =
    val repos = routed(params.repositories)
    val results = params.requests.map(r => Future(resolveRequest(params.cacheSpec, repos, r))(using requests))
    ResolveResult(results.map(Await.result(_, Duration.Inf)))

  def listVersions(params: ListVersionsParams): ListVersionsResult =
    val (org, name) = Service.module(params.module)
    val module = Module(Organization(org), ModuleName(name), Map.empty)
    val repos = routed(params.repositories)
    val cache = Recording(fileCache(params.cacheSpec))
    val versions = Versions[Task](
      cache = cache,
      moduleOpt = Some(module),
      repositories = repos,
      mirrorConfFiles = Nil,
      mirrors = Nil
    )(using Task.sync)
    val result = await(versions.result())
    val allowed = Groups.allowed(params.repositories, org)
    val perRepo = result.results.toList.collect {
      case (r: Routed, outcome) if r.allowsModule(module) =>
        // Repositories sharing this URL: named after the first of them allowed for the group.
        val id = allowed.find(r.sharing.contains).getOrElse(r.id)
        // A 404 or a missing listing is no versions. Any other failure of the metadata itself
        // (`maven-metadata.xml`) fails the request, so a missing token can't silently select a
        // lower version. Coursier's fallback, a directory listing after a 404, often isn't
        // allowed (a Nexus without browse rights, S3 without ListBucket): its failures count
        // as no listing.
        for (url, e) <- cache.entries if Service.under(url, Some(r.url)) && url.endsWith("/maven-metadata.xml")
        do
          e.left.foreach { err =>
            Recording.status(err) match
              case s @ ("401" | "403") =>
                throw ResolverError("unauthorized", s"repository `$id` refused $url: $err", UnauthorizedData(id, url, s.toInt))
              case "checksum" =>
                throw ResolverError("checksum", s"repository `$id`: $err", { val (expected, actual) = Recording.checksums(err); ChecksumData(url, expected, actual) })
              case "transport" =>
                throw ResolverError("transport", s"repository `$id`: $err", TransportData(id, url, err))
              case _ => ()
          }
        // The metadata was served but gave no versions (unparseable, say): an error too, not an
        // empty list. Only a metadata 404 with no listing is "no versions".
        val metadataServed = cache.entries.exists((url, e) =>
          Service.under(url, Some(r.url)) && url.endsWith("/maven-metadata.xml") && e.isRight
        )
        outcome.left.foreach { err =>
          if metadataServed then
            val url = cache.entries.collectFirst {
              case (u, Right(_)) if Service.under(u, Some(r.url)) && u.endsWith("/maven-metadata.xml") => u
            }.getOrElse(r.url)
            throw ResolverError("transport", s"repository `$id` served $url, which gave no versions: $err", TransportData(id, url, err))
        }
        val list = outcome.toOption.toList.flatMap(_.available0.map(_.repr)).distinct
        RepoVersions(id, list)
    }
    val merged = perRepo.flatMap(_.versions).distinct.sortBy(Version(_))
    ListVersionsResult(merged, perRepo)

  // -------------------------------------------------------------------------------------------

  /** Repositories in declared order, one coursier repository per URL, each routed by `groups`. */
  private def routed(specs: List[RepoSpec]): Seq[Routed] =
    val byUrl = specs.groupBy(s => Service.normalized(s.url))
    specs.map(s => Service.normalized(s.url)).distinct.map { url =>
      val sharing = byUrl(url)
      val underlying =
        new MavenRepository(url, None, None, versionsCheckHasModule = true, checkModule = false)
      // Repositories sharing a URL: one coursier repository, allowed for a module when any of
      // them is; results name the first of them allowed for it.
      Routed(
        underlying,
        sharing.head.id,
        m => sharing.exists(s => Groups.allowed(specs, m.organization.value).contains(s.id)),
        sharing.map(_.id),
        url
      )
    }

  /** One credential per origin and realm: a repository's own (`auth`), and each realm-scoped
    * candidate (`challenges`), sent only after a challenge naming its realm. */
  private def credentials(specs: List[RepoSpec]): Seq[DirectCredentials] =
    specs.flatMap { s =>
      Service.origin(s.url).toList.flatMap(o => (s.auth.toList ++ s.challenges).map(a => (o, a)))
    }.distinctBy((o, a) => (o, a.realm)).map { (origin, a) =>
      DirectCredentials(
        host = origin,
        usernameOpt = Some(a.user),
        passwordOpt = Some(Password(a.password)),
        realm = a.realm,
        optional = true,
        matchHost = true,
        httpsOnly = false,
        passOnRedirect = false
      )
    }

  private[resolver] def fileCache(spec: CacheSpec): FileCache[Task] =
    val policies =
      if spec.offline then Seq(CachePolicy.LocalOnly)
      else if spec.refresh then Seq(CachePolicy.UpdateChanging, CachePolicy.FetchMissing)
      else Seq(CachePolicy.LocalUpdateChanging, CachePolicy.FetchMissing)
    FileCache[Task](
      location = File(spec.cache),
      cachePolicies = policies,
      checksums = Seq(Some("SHA-1"), None),
      credentials = credentials(spec.repositories),
      logger = CacheLogger.nop,
      pool = pool,
      // `refresh`: changing metadata is always re-checked, whatever the TTL.
      ttl = Some(if spec.refresh then Duration.Zero else spec.ttl.fold(24.hours)(_.seconds)),
      localArtifactsShouldBeCached = false,
      followHttpToHttpsRedirections = true,
      followHttpsToHttpRedirections = false,
      maxRedirections = Some(20),
      sslRetry = 5,
      sslSocketFactoryOpt = Some(sslFactory),
      hostnameVerifierOpt = None,
      retry = 5,
      bufferSize = 1024 * 1024,
      classLoaders = Nil,
      clock = Clock.systemDefaultZone(),
      retryBackoffInitialDelay = 10.milliseconds,
      retryBackoffMultiplier = 2.0,
      retryBackoffMaxDelay = Some(20.seconds),
      retryPollMaxDelay = Some(200.milliseconds),
      connectTimeout = Some(30.seconds),
      readTimeout = Some(1.minute),
      userAgent = userAgent,
      hostThrottle = throttle,
      maxThrottleWait = Some(1.minute),
      authRealmOpt = None,
      rejectNonAuthoritativeResponses = false,
      httpRetryBackoffInitialDelay = 1.second,
      httpRetryBackoffMaxDelay = Some(10.minutes),
      allowCacheSubstitution = false
    )(using Task.sync)

  private def await[A](task: Task[A]): A = Await.result(task.future()(using pool.ec), Duration.Inf)

  extension (p: ExecutorService) private def ec: ExecutionContext = ExecutionContext.fromExecutorService(p)

  private def resolveRequest(spec: CacheSpec, repos: Seq[Routed], req: ResolveRequestSpec): RequestResult =
    try
      val cache = fileCache(spec)
      val systems = req.systems.distinct.sorted
      if systems.isEmpty then throw ResolverError.plain("bad_request", s"${req.label}: no systems")
      val perSystem = systems.map(s => Future(s -> resolveSystem(spec, repos, cache, req, s))(using systemsEc))
      Merge.merge(perSystem.map(Await.result(_, Duration.Inf)).toMap) match
        case Right(list) => RequestResult(classpath = Some(list))
        case Left(by)    => RequestResult(bySystem = Some(by))
    catch
      case e: ResolverError => RequestResult(error = Some(e.body))
      case NonFatal(e) =>
        e.printStackTrace()
        RequestResult(error = Some(ErrorBody("internal", e.toString)))

  /** One system's classpath for a request, in coursier's order, natives next to their module. */
  private def resolveSystem(
      spec: CacheSpec,
      repos: Seq[Routed],
      cache: FileCache[Task],
      req: ResolveRequestSpec,
      system: String
  ): List[Entry] =
    val recording = Recording(cache)
    val params = Service.resolutionParams(
      osInfo = Systems.osInfo(system),
      jdk = req.jdk,
      forced = req.forced.map(Service.coordinate).map((o, n, v) => Service.mod(o, n) -> VersionConstraint(v)).toMap
    )
    val natives = req.natives.map(n => (Service.module(n.module), n.versions.map(Interval.parse), n.natives.get(system)))
    // Native jar URL → (the module's main jar URL, its classifier).
    val nativeOf = java.util.concurrent.ConcurrentHashMap[String, (String, String)]()
    val nativesHook: Seq[(Dependency, Either[VariantPublication, Publication], Artifact)] => Seq[Artifact] =
      _.flatMap {
        case (dep, Right(pub), art) if pub.classifier.isEmpty && art.url.endsWith(".jar") =>
          val m = (dep.module.organization.value, dep.module.name.value)
          natives.collectFirst {
            case (`m`, range, Some(classifier)) if range.forall(_.contains(Version(dep.versionConstraint.asString))) =>
              val url = art.url.stripSuffix(".jar") + s"-$classifier.jar"
              nativeOf.put(url, (art.url, classifier))
              art.copy(
                url = url,
                checksumUrls = art.checksumUrls.map((k, v) => k -> (url + v.stripPrefix(art.url))),
                extra = Map.empty,
                // Optional: a mapped jar the repository answers 404 or 403 for (S3 answers 403 for a
                // missing object without ListBucket) leaves that system without one
                // (scpm notes it) instead of failing the whole resolution.
                optional = true
              )
          }
        case _ => Nil
      }
    val resolve = Resolve[Task](
      cache = recording,
      dependencies = req.roots.map(Service.dependency),
      repositories = repos,
      mirrorConfFiles = Nil,
      mirrors = Nil,
      resolutionParams = params,
      throughOpt = None,
      transformFetcherOpt = None,
      initialResolution = None,
      confFiles = Nil,
      preferConfFileDefaultRepositories = false,
      mapDependenciesOpt = None,
      bomDependencies = Nil,
      bomModuleVersions = Nil,
      boms = req.boms.map(Service.coordinate).map((o, n, v) =>
        BomDependency(Service.mod(o, n), VersionConstraint(v), Configuration.empty, forceOverrideVersions = false)
      ),
      gradleModuleSupport = None
    )(using Task.sync)
    val artifacts = Artifacts[Task](
      cache = cache,
      resolutions = Nil,
      classifiers = Set.empty,
      mainArtifactsOpt = None,
      artifactTypesOpt = Some(req.artifactTypes.map(Type(_)).toSet).filter(_.nonEmpty),
      otherCaches = Nil,
      extraArtifactsSeq = Nil,
      classpathOrder = true,
      transformArtifacts = Nil,
      transformArtifacts0 = Nil,
      extraArtifactsSeq0 = Seq(nativesHook),
      attributes = Nil
    )(using Task.sync)
    val result =
      try await(new Fetch[Task](resolve, artifacts, None).ioResult)
      catch case e: CoursierError => throw Service.error(e, spec.repositories, recording, spec.offline)
    val repoIdOf = (url: String, group: String) => Service.repositoryOf(spec.repositories, url, group)
    val main = result.fullDetailedArtifacts0.toList.collect { case (dep, pub, art, Some(file)) =>
      val classifier = pub.toOption.map(_.classifier.value).filter(_.nonEmpty)
      val group = dep.module.organization.value
      Entry(
        group = group,
        artifact = dep.module.name.value,
        version = dep.versionConstraint.asString,
        classifier = classifier,
        extension = Service.extension(file),
        url = art.url,
        file = file.getAbsolutePath,
        repository = repoIdOf(art.url, group)
      )
    }
    // A root asking for a specific artifact (a classifier or a non-jar type) must yield a file.
    for root <- req.roots do
      val (o, n) = Service.module(root.module)
      val kind = root.`type`.filterNot(Set("jar", "bundle", "pom"))
      if root.classifier.isDefined || kind.isDefined then
        val want = root.classifier.getOrElse(if root.`type`.contains("test-jar") then "tests" else "")
        if !main.exists(e => e.group == o && e.artifact == n && e.classifier.getOrElse("") == want) then
          throw ResolverError(
            "no_artifact",
            s"${root.module}:${root.version} resolved to no file for classifier `$want`",
            NoArtifactData(s"${root.module}:${root.version}")
          )
    val extras = result.fullExtraArtifacts.toList.collect {
      case (art, Some(file)) if nativeOf.containsKey(art.url) => art.url -> file
    }.toMap
    main.flatMap { e =>
      val natives = extras.toList.sortBy(_._1).collect {
        case (url, file) if nativeOf.get(url)._1 == e.url =>
          e.copy(
            classifier = Some(nativeOf.get(url)._2),
            extension = Service.extension(file),
            url = url,
            file = file.getAbsolutePath
          )
      }
      e :: natives
    }

object Service:
  /** Downloads and resolution work share one pool, sized for the total concurrency a re-lock had
    * with `cs` (8 processes × 6), not coursier's per-process 6. */
  val PoolSize = 48

  /** Requests, and systems of requests, resolved at once; more wait their turn. */
  val Orchestration: Int = math.max(4, Runtime.getRuntime.availableProcessors)

  def daemonFactory(name: String): java.util.concurrent.ThreadFactory = r =>
    val t = Thread(r, name)
    t.setDaemon(true)
    t

  def daemonPool(size: Int, name: String): ExecutorService = Executors.newFixedThreadPool(size, daemonFactory(name))

  /** Every field of `ResolutionParams` given; OS, arch and JDK from the request, never the JVM's. */
  def resolutionParams(osInfo: Activation.Os, jdk: String, forced: Map[Module, VersionConstraint]): ResolutionParams =
    ResolutionParams(
      keepOptionalDependencies = false,
      maxIterations = 200,
      forceVersion0 = forced,
      forcedProperties = Map.empty,
      profiles = Set.empty,
      scalaVersionOpt0 = None,
      forceScalaVersionOpt = None,
      typelevel = false,
      rules = Seq.empty,
      reconciliation0 = Nil,
      properties = Nil,
      exclusions = Set.empty,
      osInfoOpt = Some(osInfo),
      jdkVersionOpt0 = Some(Version(jdk)),
      useSystemOsInfo = false,
      useSystemJdkVersion = false,
      defaultConfiguration = Configuration.defaultRuntime,
      overrideFullSuffixOpt = None,
      keepProvidedDependencies = None,
      forceDepMgmtVersions = None,
      enableDependencyOverrides = None,
      defaultVariantAttributes = None,
      renderModuleVersion = None,
      scalaOrganizationOverride = None,
      ignoreOptionalFromDepMgmt = false
    )

  def mod(org: String, name: String): Module = Module(Organization(org), ModuleName(name), Map.empty)

  def module(s: String): (String, String) = s.split(':') match
    case Array(o, n) if o.nonEmpty && n.nonEmpty => (o, n)
    case _ => throw ResolverError.plain("bad_request", s"expected org:name, got `$s`")

  def coordinate(s: String): (String, String, String) = s.split(':') match
    case Array(o, n, v) if o.nonEmpty && n.nonEmpty && v.nonEmpty => (o, n, v)
    case _ => throw ResolverError.plain("bad_request", s"expected org:name:version, got `$s`")

  /** A root as `cs` builds it from `org:name:version,classifier=…,type=…,exclude=…`. */
  def dependency(root: RootSpec): Dependency =
    val (o, n) = module(root.module)
    val base = Dependency(mod(o, n), VersionConstraint(root.version))
    val publication = base.publication.copy(
      classifier = root.classifier.fold(base.publication.classifier)(Classifier(_)),
      `type` = root.`type`.fold(base.publication.`type`)(Type(_))
    )
    val exclusions = root.exclusions.map(module).map((eo, en) => (Organization(eo), ModuleName(en))).toSet
    base.copy(publication = publication, transitive = root.transitive).copy(minimizedExclusions = MinimizedExclusions(exclusions))

  def extension(file: File): String =
    val name = file.getName
    val dot = name.lastIndexOf('.')
    if dot < 0 then "jar" else name.substring(dot + 1)

  def normalized(url: String): String = url.stripSuffix("/")

  def under(url: String, root: Option[String]): Boolean = root.exists(r => url.startsWith(normalized(r) + "/"))

  /** `scheme://host[:port]` of a URL, which credentials are scoped to (as for `cs`). */
  def origin(url: String): Option[String] =
    try
      val u = URI(url)
      Option(u.getScheme).zip(Option(u.getHost)).map { (s, h) =>
        val port = if u.getPort >= 0 then s":${u.getPort}" else ""
        s"${s.toLowerCase(Locale.ROOT)}://$h$port"
      }
    catch case NonFatal(_) => None

  /** The configured repository a URL came from: the longest matching URL, then the first of
    * those allowed for the group (several share a mirror), else the first of them. */
  def repositoryOf(specs: List[RepoSpec], url: String, group: String): String =
    val matching = specs.filter(s => under(url, Some(s.url)))
    val longest = matching.map(s => normalized(s.url).length).maxOption.getOrElse(0)
    val candidates = matching.filter(s => normalized(s.url).length == longest)
    val allowed = Groups.allowed(specs, group).toSet
    candidates.find(s => allowed(s.id)).orElse(candidates.headOption).fold("")(_.id)

  /** `(org:name, version)` of an artifact URL in a Maven layout under one of `specs`. */
  def coordinatesOf(specs: List[RepoSpec], url: String): Option[(String, String)] =
    specs.find(s => under(url, Some(s.url))).flatMap { s =>
      url.stripPrefix(normalized(s.url) + "/").split('/').toList.reverse match
        case _ :: version :: name :: group if group.nonEmpty =>
          Some((group.reverse.mkString(".") + ":" + name, version))
        case _ => None
    }

  /** A protocol error for a coursier failure; `tried` from what the recording saw, and
    * `offline` for whether the request was offline (then a miss is `offline_miss`). */
  def error(e: CoursierError, specs: List[RepoSpec], recording: Recording, offline: Boolean): ResolverError =
    def repoOf(url: String) = specs.find(s => under(url, Some(s.url))).fold("")(_.id)
    // The POM a module comes from in the first repository allowed for it (offline, nothing is
    // recorded, since nothing is asked).
    def pomUrl(c: ResolutionError.CantDownloadModule): Option[String] =
      val (org, name, version) = (c.module.organization.value, c.module.name.value, c.versionConstraint.asString)
      Groups.allowed(specs, org).headOption.flatMap(id => specs.find(_.id == id)).map { s =>
        s"${normalized(s.url)}/${org.replace('.', '/')}/$name/$version/$name-$version.pom"
      }
    // Requests about the module, anchored at a path segment (`/org/acme/name/`, not `xorg/…`).
    def tried(c: ResolutionError.CantDownloadModule) =
      val path = "/" + c.module.organization.value.replace('.', '/') + "/" + c.module.name.value + "/"
      recording.entries.collect {
        case (url, outcome) if url.contains(path) => Tried(repoOf(url), url, outcome.fold(Recording.status, _ => "200"))
      }
    e match
      case r: ResolutionError =>
        val failures = r.errors.collect { case c: ResolutionError.CantDownloadModule => (c, tried(c)) }
        // The most telling failure: refused credentials hide behind any other module's 404.
        val refused = failures.iterator.flatMap((_, ts) => ts.find(t => t.status == "401" || t.status == "403")).nextOption()
        val checksum = failures.iterator.flatMap((_, ts) => ts.find(_.status == "checksum")).nextOption()
        val transport = failures.iterator.flatMap((_, ts) => ts.find(_.status == "transport")).nextOption()
        (refused, checksum, transport, failures.headOption) match
          case (Some(t), _, _, _) =>
            ResolverError("unauthorized", s"repository `${t.repository}` refused ${t.url} (HTTP ${t.status})", UnauthorizedData(t.repository, t.url, t.status.toInt))
          case (_, Some(t), _, _) =>
            val text = recording.entries.collectFirst { case (u, Left(e)) if u == t.url && Recording.status(e) == "checksum" => e }.getOrElse("")
            val (expected, actual) = Recording.checksums(text)
            ResolverError("checksum", s"${t.url}: a served checksum didn't match ($text)", ChecksumData(t.url, expected, actual))
          case (_, _, Some(t), Some((c, _))) =>
            ResolverError("transport", s"cannot reach ${t.url}: ${c.perRepositoryErrors.mkString("; ")}", TransportData(t.repository, t.url, c.perRepositoryErrors.mkString("; ")))
          case (_, _, _, Some((c, attempts))) if offline =>
            ResolverError("offline_miss", s"${c.module.repr}:${c.versionConstraint.asString} is not cached", OfflineMissData(attempts.headOption.map(_.url).orElse(pomUrl(c)).getOrElse(c.module.repr)))
          case (_, _, _, Some((c, attempts))) =>
            ResolverError("not_found", r.getMessage, NotFoundData(c.module.repr, c.versionConstraint.asString, attempts))
          case _ => ResolverError.plain("internal", r.getMessage)
      case f: FetchError.DownloadingArtifacts =>
        val (art, err) = f.errors.head
        val (module, version) = coordinatesOf(specs, art.url).getOrElse((art.url, ""))
        err match
          case _: ArtifactError.Unauthorized | _: ArtifactError.Forbidden =>
            val status = if err.isInstanceOf[ArtifactError.Forbidden] then 403 else 401
            ResolverError("unauthorized", err.describe, UnauthorizedData(repoOf(art.url), art.url, status))
          case _: ArtifactError.NotFound | _: ArtifactError.FileTooOldOrNotFound if offline =>
            ResolverError("offline_miss", err.describe, OfflineMissData(art.url))
          case _: ArtifactError.NotFound =>
            ResolverError("not_found", err.describe, NotFoundData(module, version, List(Tried(repoOf(art.url), art.url, "404"))))
          case w: ArtifactError.WrongChecksum =>
            ResolverError("checksum", err.describe, ChecksumData(art.url, w.expected, w.got))
          case _: ArtifactError.FileTooOldOrNotFound =>
            ResolverError("offline_miss", err.describe, OfflineMissData(art.url))
          case _ =>
            ResolverError("transport", err.describe, TransportData(repoOf(art.url), art.url, err.describe))
      case other => ResolverError.plain("internal", other.getMessage)
