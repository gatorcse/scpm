package scpm.resolver

import coursier.core.{ ArtifactSource, Module, Project, Repository, Versions }
import coursier.util.{ EitherT, Monad }
import coursier.version.{ Version => Version0 }

import scala.annotation.nowarn

/** Which repositories serve a group (design §5.4, `groups:`), as Rust's `allowed_repos` decides. */
object Groups:
  /** `prefix` covers `group` when equal, or when `group` continues it after a dot. */
  def covers(prefix: String, group: String): Boolean =
    group == prefix || group.startsWith(prefix + ".")

  /** The ids of the repositories `group` comes from, in declared order: those whose `groups`
    * have the longest prefix covering it (several on a tie), else every repository without
    * `groups`. Empty when every repository has `groups` and none covers it. */
  def allowed(repos: Seq[RepoSpec], group: String): Seq[String] =
    def longest(r: RepoSpec): Option[Int] =
      r.groups.getOrElse(Nil).filter(covers(_, group)).map(_.length).maxOption
    repos.flatMap(longest).maxOption match
      case Some(len) => repos.filter(r => longest(r).contains(len)).map(_.id)
      case None      => repos.filter(_.groups.forall(_.isEmpty)).map(_.id)

/**
 * A repository that serves only the modules `allows` accepts (design §5.2.1, "`groups` inside
 * resolution"). For any other module it answers not-found without making a request, both when
 * asked for a module and for its versions. `find0` returns the underlying repository as the
 * `ArtifactSource`, so artifacts (Gradle module ones included) come from it.
 */
final class Routed(val underlying: Repository, val id: String, allows: Module => Boolean) extends Repository:
  override def repr: String = underlying.repr

  private def denied[F[_], A](module: Module)(using F: Monad[F]): EitherT[F, String, A] =
    EitherT(F.point(Left(s"${module.repr} is not routed to $id")))

  override def find0[F[_]](module: Module, version: Version0, fetch: Repository.Fetch[F])(using
      F: Monad[F]
  ): EitherT[F, String, (ArtifactSource, Project)] =
    if allows(module) then underlying.find0(module, version, fetch) else denied(module)

  @nowarn("cat=deprecation")
  override def find[F[_]](module: Module, version: String, fetch: Repository.Fetch[F])(using
      F: Monad[F]
  ): EitherT[F, String, (ArtifactSource, Project)] =
    if allows(module) then underlying.find(module, version, fetch) else denied(module)

  override def versions[F[_]](module: Module, fetch: Repository.Fetch[F])(using
      F: Monad[F]
  ): EitherT[F, String, (Versions, String)] =
    if allows(module) then underlying.versions(module, fetch) else denied(module)

  override def versions[F[_]](module: Module, fetch: Repository.Fetch[F], versionsCheckHasModule: Boolean)(using
      F: Monad[F]
  ): EitherT[F, String, (Versions, String)] =
    if allows(module) then underlying.versions(module, fetch, versionsCheckHasModule) else denied(module)

  override def versionsCheckHasModule: Boolean = underlying.versionsCheckHasModule

  // Nothing in the sidecar uses `Complete`, and it must not bypass routing.
  override def completeOpt[F[_]: Monad](fetch: Repository.Fetch[F]): Option[Repository.Complete[F]] = None

  override def artifacts(
      dependency: coursier.core.Dependency,
      project: Project,
      overrideClassifiers: Option[Seq[coursier.core.Classifier]]
  ): Seq[(coursier.core.Publication, coursier.util.Artifact)] =
    underlying.artifacts(dependency, project, overrideClassifiers)
