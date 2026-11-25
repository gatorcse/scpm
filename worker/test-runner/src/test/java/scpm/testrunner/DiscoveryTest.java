package scpm.testrunner;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;

import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.util.Optional;
import org.junit.Test;
import sbt.testing.AnnotatedFingerprint;
import sbt.testing.Fingerprint;
import sbt.testing.SubclassFingerprint;

public class DiscoveryTest {
  public abstract static class BaseSuite {}

  public static class ConcreteSuite extends BaseSuite {}

  public abstract static class AbstractSuite extends BaseSuite {}

  /** Shaped like a Scala object's class: name ends in $, private constructor. */
  public static final class Module$ extends BaseSuite {
    private Module$() {}
  }

  public static class NoDefaultConstructor extends BaseSuite {
    public NoDefaultConstructor(int x) {}
  }

  @Retention(RetentionPolicy.RUNTIME)
  public @interface Check {}

  public static class Annotated {
    @Check
    public void works() {}
  }

  private static final String P = DiscoveryTest.class.getName() + "$";

  private static Fingerprint subclass(boolean module) {
    return new SubclassFingerprint() {
      public boolean isModule() { return module; }
      public String superclassName() { return P + "BaseSuite"; }
      public boolean requireNoArgConstructor() { return true; }
    };
  }

  private static final Fingerprint ANNOTATED =
      new AnnotatedFingerprint() {
        public boolean isModule() { return false; }
        public String annotationName() { return P + "Check"; }
      };

  private final Discovery discovery = new Discovery(DiscoveryTest.class.getClassLoader());

  @Test
  public void matchesConcreteSubclasses() {
    Fingerprint fp = subclass(false);
    assertEquals(Optional.of(fp), discovery.match(P + "ConcreteSuite", new Fingerprint[] {fp}));
  }

  @Test
  public void skipsAbstractClassesAndThoseWithoutNoArgConstructor() {
    Fingerprint[] fps = {subclass(false)};
    assertTrue(discovery.match(P + "AbstractSuite", fps).isEmpty());
    assertTrue(discovery.match(P + "NoDefaultConstructor", fps).isEmpty());
    assertTrue(discovery.match(P + "BaseSuite", fps).isEmpty());
  }

  @Test
  public void moduleFingerprintsOnlyMatchObjects() {
    assertTrue(discovery.match(P + "ConcreteSuite", new Fingerprint[] {subclass(true)}).isEmpty());
  }

  @Test
  public void objectsMatchDespiteTheirPrivateConstructor() {
    Fingerprint fp = subclass(true);
    assertEquals(Optional.of(fp), discovery.match(P + "Module$", new Fingerprint[] {fp}));
  }

  @Test
  public void matchesAnnotatedMethods() {
    assertEquals(Optional.of(ANNOTATED), discovery.match(P + "Annotated", new Fingerprint[] {ANNOTATED}));
  }

  @Test
  public void stripsTheModuleSuffix() {
    assertEquals("a.Foo", Discovery.testName("a.Foo$"));
    assertEquals("a.Foo", Discovery.testName("a.Foo"));
  }
}
