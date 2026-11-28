package scpm.resolver

import java.io.{ FileInputStream, IOException }
import java.net.{ InetSocketAddress, Proxy, ProxySelector, SocketAddress, URI }
import java.security.KeyStore
import java.security.cert.{ CertificateFactory, X509Certificate }
import java.util.Locale
import javax.net.ssl.{ SSLContext, SSLSocketFactory, TrustManagerFactory }
import scala.jdk.CollectionConverters.*
import scala.util.Using
import scala.util.control.NonFatal

/**
 * TLS trust from the OS trust store (design §5.2.1), as the Rust fetcher has it: the macOS
 * Keychain, Windows' `Windows-ROOT`, or on Linux the system PEM bundle scpm names. Where the OS
 * store can't be read whole, the JDK's `cacerts` is added (see [[Trust.select]]).
 */
object Trust:
  /** The certificates to trust, and a note when the OS store wasn't enough on its own:
    *
    *   - macOS: `KeychainStore-ROOT` (the system roots, JDK 23+) and `KeychainStore` (the user's
    *     keychain, which holds no public roots). Without `KeychainStore-ROOT` (JDK 17–22), the
    *     JDK's `cacerts` stands in for the system roots.
    *   - Windows: `Windows-ROOT`, plus `cacerts`. Windows fills its root store lazily (a root is
    *     added when Windows' own chain building first needs it), and the JDK's `Windows-ROOT`
    *     reads the store without triggering that, so a public root may simply not be there yet.
    *   - Linux: the PEM bundle; without one, `cacerts`.
    */
  def select[C](
      os: String,
      load: String => Option[Seq[C]],
      pemBundle: Option[String],
      readPem: String => Option[Seq[C]],
      jdk: => Seq[C]
  ): (Seq[C], Option[String]) =
    val name = os.toLowerCase(Locale.ROOT)
    if name.contains("mac") then
      val user = load("KeychainStore").getOrElse(Nil)
      load("KeychainStore-ROOT") match
        case Some(roots) => (roots ++ user, None)
        case None =>
          (jdk ++ user, Some("this JDK has no KeychainStore-ROOT (JDK 23+); using its cacerts and the user keychain"))
    else if name.contains("windows") then (load("Windows-ROOT").getOrElse(Nil) ++ jdk, None)
    else
      pemBundle.flatMap(p => readPem(p).filter(_.nonEmpty)) match
        case Some(certs) => (certs, None)
        case None        => (jdk, Some("no system PEM bundle was found; using the JDK's cacerts"))

  def socketFactory(spec: Option[TrustSpec]): SSLSocketFactory =
    val (certs, note) =
      select(System.getProperty("os.name", ""), fromStore, spec.flatMap(_.pemBundle), fromPem, jdkCertificates)
    note.foreach(n => System.err.println(s"scpm resolver: $n"))
    val store = KeyStore.getInstance(KeyStore.getDefaultType)
    store.load(null, null)
    certs.zipWithIndex.foreach((c, i) => store.setCertificateEntry(s"trusted-$i", c))
    val tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm)
    tmf.init(store)
    val ctx = SSLContext.getInstance("TLS")
    ctx.init(null, tmf.getTrustManagers, null)
    ctx.getSocketFactory

  /** The JDK's default trust anchors (`cacerts`). */
  private def jdkCertificates: Seq[X509Certificate] =
    val tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm)
    tmf.init(null: KeyStore)
    tmf.getTrustManagers.toSeq.collect { case x: javax.net.ssl.X509TrustManager => x.getAcceptedIssuers.toSeq }.flatten

  private def fromStore(kind: String): Option[Seq[X509Certificate]] =
    try
      val store = KeyStore.getInstance(kind)
      store.load(null, null)
      Some(store.aliases.asScala.toSeq.flatMap(a => Option(store.getCertificate(a))).collect {
        case c: X509Certificate => c
      })
    catch case NonFatal(_) => None

  private def fromPem(path: String): Option[Seq[X509Certificate]] =
    try
      Some(Using.resource(FileInputStream(path)) { in =>
        CertificateFactory.getInstance("X.509").generateCertificates(in).asScala.toSeq.collect {
          case c: X509Certificate => c
        }
      })
    catch case _: IOException | _: java.security.cert.CertificateException => None

/**
 * The JVM-wide proxy selector, from scpm's resolved settings (design §5.9): `noProxy` entries
 * cover a host and its subdomains, `*` covers every host, and loopback is always direct (scpm
 * sends it explicitly). The same matcher as Rust's `ProxySettings::bypasses`.
 */
final class Proxies(spec: ProxySpec) extends ProxySelector:
  def bypasses(host: String): Boolean =
    val h = host.stripPrefix("[").stripSuffix("]").toLowerCase(Locale.ROOT)
    spec.noProxy.exists { e =>
      val domain = e.dropWhile(_ == '.')
      e == "*" || h == domain || h.endsWith("." + domain)
    }

  override def select(uri: URI): java.util.List[Proxy] =
    val scheme = Option(uri.getScheme).map(_.toLowerCase(Locale.ROOT))
    val server = scheme match
      case Some("https") => spec.https
      case Some("http")  => spec.http
      case _             => None
    val proxy = server.filterNot(_ => Option(uri.getHost).exists(bypasses)) match
      case Some(s) => Proxy(Proxy.Type.HTTP, InetSocketAddress.createUnresolved(s.host, s.port))
      case None    => Proxy.NO_PROXY
    java.util.List.of(proxy)

  override def connectFailed(uri: URI, sa: SocketAddress, ioe: IOException): Unit = ()
