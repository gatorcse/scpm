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
 * Keychain, Windows' `Windows-ROOT`, or on Linux the system PEM bundle scpm names. The JDK's own
 * `cacerts` is used only when the OS store yields no certificate at all (a JDK without the
 * Keychain root store, or no bundle), with a note on stderr.
 */
object Trust:
  def socketFactory(spec: Option[TrustSpec]): SSLSocketFactory =
    val certs = osCertificates(spec.flatMap(_.pemBundle))
    if certs.isEmpty then
      System.err.println("scpm resolver: the OS trust store gave no certificates; using the JDK's")
      SSLContext.getDefault.getSocketFactory
    else
      val store = KeyStore.getInstance(KeyStore.getDefaultType)
      store.load(null, null)
      certs.zipWithIndex.foreach((c, i) => store.setCertificateEntry(s"os-$i", c))
      val tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm)
      tmf.init(store)
      val ctx = SSLContext.getInstance("TLS")
      ctx.init(null, tmf.getTrustManagers, null)
      ctx.getSocketFactory

  private def osCertificates(pemBundle: Option[String]): Seq[X509Certificate] =
    val os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT)
    if os.contains("mac") then Seq("KeychainStore-ROOT", "KeychainStore").flatMap(fromStore)
    else if os.contains("windows") then fromStore("Windows-ROOT")
    else pemBundle.toSeq.flatMap(fromPem)

  private def fromStore(kind: String): Seq[X509Certificate] =
    try
      val store = KeyStore.getInstance(kind)
      store.load(null, null)
      store.aliases.asScala.toSeq.flatMap(a => Option(store.getCertificate(a))).collect { case c: X509Certificate => c }
    catch case NonFatal(_) => Nil

  private def fromPem(path: String): Seq[X509Certificate] =
    try
      Using.resource(FileInputStream(path)) { in =>
        CertificateFactory.getInstance("X.509").generateCertificates(in).asScala.toSeq.collect {
          case c: X509Certificate => c
        }
      }
    catch case _: IOException | _: java.security.cert.CertificateException => Nil

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
